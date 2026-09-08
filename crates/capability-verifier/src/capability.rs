// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;

use biscuit_auth::{
    Biscuit, PublicKey,
    builder::{Algorithm, AuthorizerBuilder},
};
use prost::Message;

use crate::{
    Error, Result, VerificationLimits,
    canonical::{OWNER_CAPABILITY_DOMAIN, capability_body, capability_without_id, digest, key_id},
    crypto::verify_signature,
    owner::{VerifiedOwnerState, apply_transition, verify_owner_root},
    wire::{
        AuthorizationKeyAlgorithm, CapabilityPrincipalKind, OwnerAuthorizationBundle,
        OwnerCapability, SignedOwnerCapability, SpoolCapabilityAction, SpoolCapabilityGrant,
        SpoolSelector,
    },
};

const OWNER_RULES: &str = include_str!("rules.biscuit");

/// Capability whose id, lifetime, exact purge scope, and signature are verified.
#[derive(Clone)]
pub struct VerifiedCapability {
    signed: SignedOwnerCapability,
}

impl VerifiedCapability {
    /// Verified body.
    #[must_use]
    pub fn capability(&self) -> &OwnerCapability {
        self.signed
            .capability
            .as_ref()
            .expect("verified capability body")
    }

    /// Stable owner id.
    #[must_use]
    pub fn owner_id(&self) -> [u8; 32] {
        self.capability()
            .owner_id
            .as_slice()
            .try_into()
            .expect("verified owner id")
    }

    /// State hash named by the capability issuer.
    #[must_use]
    pub fn issuer_state_hash(&self) -> [u8; 32] {
        self.capability()
            .issuer_state_hash
            .as_slice()
            .try_into()
            .expect("verified state hash")
    }

    /// Signed wire object.
    #[must_use]
    pub const fn signed(&self) -> &SignedOwnerCapability {
        &self.signed
    }
}

/// Fully verified portable root/state/direct-purge-capability/Biscuit bundle.
pub struct VerifiedAuthorizationBundle {
    owner_state: VerifiedOwnerState,
    capability: VerifiedCapability,
}

impl VerifiedAuthorizationBundle {
    /// Accepted owner state.
    #[must_use]
    pub const fn owner_state(&self) -> &VerifiedOwnerState {
        &self.owner_state
    }

    /// Direct purge capability bound to the subject Biscuit.
    #[must_use]
    pub const fn capability(&self) -> &VerifiedCapability {
        &self.capability
    }
}

pub(crate) fn validate_path_segments(segments: &[String]) -> Result<()> {
    if segments.len() > VerificationLimits::MAX_PATH_SEGMENTS {
        return Err(Error::TooLarge {
            limit: VerificationLimits::MAX_PATH_SEGMENTS,
        });
    }
    if segments.iter().any(|segment| {
        segment.is_empty()
            || segment.len() > VerificationLimits::MAX_PATH_SEGMENT_BYTES
            || matches!(segment.as_str(), "." | "..")
            || segment.contains('/')
            || segment.contains('\0')
    }) {
        return Err(Error::Invalid(
            "spool path contains a non-canonical segment".to_owned(),
        ));
    }
    Ok(())
}

fn validate_selector(selector: &SpoolSelector) -> Result<()> {
    if selector.root_spool_uuid.len() != 16 {
        return Err(Error::Invalid(
            "spool selector UUID must be 16 bytes".to_owned(),
        ));
    }
    validate_path_segments(&selector.path_segments)?;
    if selector.include_descendants {
        return Err(Error::CapabilityDenied(
            "purge requires an exact spool selector".to_owned(),
        ));
    }
    Ok(())
}

fn validate_grant(grant: &SpoolCapabilityGrant) -> Result<()> {
    let selector = grant
        .spool
        .as_ref()
        .ok_or_else(|| Error::Invalid("capability grant has no selector".to_owned()))?;
    validate_selector(selector)?;
    if SpoolCapabilityAction::try_from(grant.action).ok() != Some(SpoolCapabilityAction::Purge) {
        return Err(Error::CapabilityDenied(
            "owner capability grant action is not PURGE".to_owned(),
        ));
    }
    Ok(())
}

fn capability_is_well_formed(
    capability: &OwnerCapability,
    limits: VerificationLimits,
) -> Result<()> {
    if capability.grants.len() > VerificationLimits::MAX_GRANTS {
        return Err(Error::TooLarge {
            limit: VerificationLimits::MAX_GRANTS,
        });
    }
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
        return Err(Error::Invalid(
            "owner capability has invalid v1 fields or lifetime".to_owned(),
        ));
    }
    if !capability.parent_capability_id.is_empty() {
        return Err(Error::CapabilityDenied(
            "purge is direct-only and cannot name a parent capability".to_owned(),
        ));
    }
    let subject = capability
        .subject
        .as_ref()
        .ok_or_else(|| Error::Invalid("owner capability has no subject".to_owned()))?;
    let kind = CapabilityPrincipalKind::try_from(subject.kind)
        .ok()
        .filter(|kind| *kind != CapabilityPrincipalKind::Unspecified)
        .ok_or_else(|| Error::Invalid("unknown capability principal".to_owned()))?;
    match (kind, &subject.key) {
        (CapabilityPrincipalKind::AnyAnonymous, None) if subject.principal_id.is_empty() => {}
        (CapabilityPrincipalKind::AnyAnonymous, _) => {
            return Err(Error::Invalid(
                "ANY_ANONYMOUS must omit key and principal id".to_owned(),
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
            return Err(Error::Invalid(
                "capability subject key or principal id is invalid".to_owned(),
            ));
        }
    }
    for grant in &capability.grants {
        validate_grant(grant)?;
    }
    Ok(())
}

fn request_matches_selector(
    granted: &SpoolSelector,
    requested_spool_uuid: &[u8; 16],
    requested_path: &[String],
) -> bool {
    granted.root_spool_uuid.as_slice() == requested_spool_uuid
        && !granted.include_descendants
        && granted.path_segments == requested_path
}

pub(crate) fn capability_allows_purge(
    capability: &OwnerCapability,
    spool_uuid: &[u8; 16],
    path: &[String],
) -> bool {
    capability.grants.iter().any(|grant| {
        grant.action == SpoolCapabilityAction::Purge as i32
            && grant
                .spool
                .as_ref()
                .is_some_and(|selector| request_matches_selector(selector, spool_uuid, path))
    })
}

fn path_hex(segments: &[String]) -> String {
    let mut bytes = Vec::new();
    for segment in segments {
        bytes.extend_from_slice(&(segment.len() as u32).to_be_bytes());
        bytes.extend_from_slice(segment.as_bytes());
    }
    hex::encode(bytes)
}

fn expected_grants(capability: &OwnerCapability) -> Result<BTreeSet<(String, String, bool, i64)>> {
    capability
        .grants
        .iter()
        .map(|grant| {
            let selector = grant
                .spool
                .as_ref()
                .ok_or_else(|| Error::Invalid("capability grant has no selector".to_owned()))?;
            Ok((
                hex::encode(&selector.root_spool_uuid),
                path_hex(&selector.path_segments),
                selector.include_descendants,
                i64::from(grant.action),
            ))
        })
        .collect()
}

fn verify_subject_biscuit(capability: &OwnerCapability, bytes: &[u8]) -> Result<()> {
    let subject = capability
        .subject
        .as_ref()
        .ok_or_else(|| Error::Invalid("capability has no subject".to_owned()))?;
    let kind = CapabilityPrincipalKind::try_from(subject.kind)
        .map_err(|_| Error::Invalid("unknown subject kind".to_owned()))?;
    if kind == CapabilityPrincipalKind::AnyAnonymous {
        return if bytes.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid(
                "ANY_ANONYMOUS bundle must omit subject Biscuit".to_owned(),
            ))
        };
    }
    let key = subject
        .key
        .as_ref()
        .ok_or_else(|| Error::Invalid("capability subject has no key".to_owned()))?;
    let public = PublicKey::from_bytes(&key.public_key, Algorithm::Ed25519)
        .map_err(|error| Error::Biscuit(error.to_string()))?;
    let biscuit = Biscuit::from(bytes, move |_| Ok(public))
        .map_err(|error| Error::Biscuit(error.to_string()))?;
    if biscuit.block_count() != 1 {
        return Err(Error::CapabilityDenied(
            "purge is direct-only; subject Biscuit attenuation is forbidden".to_owned(),
        ));
    }
    let source = biscuit
        .print_block_source(0)
        .map_err(|error| Error::Biscuit(error.to_string()))?;
    let lines = source
        .lines()
        .map(|line| line.trim_end_matches(';').to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let actual = lines.iter().cloned().collect::<BTreeSet<_>>();
    let mut expected = BTreeSet::from([
        format!(
            "owner_subject({}, \"{}\", \"{}\")",
            subject.kind,
            hex::encode(&subject.principal_id),
            hex::encode(key_id(key))
        ),
        format!(
            "owner_capability(\"{}\")",
            hex::encode(&capability.capability_id)
        ),
        format!(
            "owner_validity({}, {})",
            capability.not_before_unix_seconds, capability.expires_at_unix_seconds
        ),
    ]);
    for (spool, path, descendants, action) in expected_grants(capability)? {
        expected.insert(format!(
            "owner_grant(\"{spool}\", \"{path}\", {descendants}, {action})"
        ));
    }
    if lines.len() != expected.len() || actual != expected {
        return Err(Error::CapabilityDenied(
            "subject Biscuit authority facts differ from the purge capability".to_owned(),
        ));
    }

    // Parse the checked-in rule pack on every verification path so an invalid
    // policy can never ship unnoticed. The exact authority-fact comparison
    // above is the decision; running a Datalog clock/limit loop would add a
    // host-dependent failure mode to Worker WASM without widening assurance.
    let _rule_pack = AuthorizerBuilder::new()
        .code(OWNER_RULES)
        .map_err(|error| Error::Biscuit(error.to_string()))?;
    Ok(())
}

/// Verify the single direct PURGE capability accepted by owner-authz v2.
pub fn verify_capability_chain(
    state: &VerifiedOwnerState,
    chain: &[SignedOwnerCapability],
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedCapability> {
    if chain.is_empty() {
        return Err(Error::CapabilityDenied(
            "authorization bundle has no purge capability".to_owned(),
        ));
    }
    if chain.len() > VerificationLimits::MAX_CAPABILITIES {
        return Err(Error::TooLarge {
            limit: VerificationLimits::MAX_CAPABILITIES,
        });
    }
    if chain.len() != 1 {
        return Err(Error::CapabilityDenied(
            "purge is direct-only and cannot be attenuated".to_owned(),
        ));
    }
    let signed = &chain[0];
    let capability = signed
        .capability
        .as_ref()
        .ok_or_else(|| Error::Invalid("signed capability has no body".to_owned()))?;
    capability_is_well_formed(capability, limits)?;
    let expected = digest(OWNER_CAPABILITY_DOMAIN, &capability_without_id(capability)?);
    if capability.capability_id.as_slice() != expected {
        return Err(Error::Invalid(
            "capability id does not match canonical body".to_owned(),
        ));
    }
    if capability.owner_id.as_slice() != state.owner_id() {
        return Err(Error::BrokenChain(
            "capability names another owner".to_owned(),
        ));
    }
    if now_unix_seconds < capability.not_before_unix_seconds {
        return Err(Error::NotYetValid);
    }
    if now_unix_seconds > capability.expires_at_unix_seconds {
        return Err(Error::Expired);
    }
    let signer = state.issuer_at(&capability.issuer_state_hash, now_unix_seconds)?;
    verify_signature(
        signer,
        signed.signature.as_ref().ok_or(Error::InvalidSignature)?,
        OWNER_CAPABILITY_DOMAIN,
        &capability_body(capability)?,
    )?;
    Ok(VerifiedCapability {
        signed: signed.clone(),
    })
}

/// Verify a portable owner-root/state/direct-purge-capability/Biscuit bundle.
pub fn verify_authorization_bundle(
    bundle: &OwnerAuthorizationBundle,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedAuthorizationBundle> {
    if bundle.encoded_len() > limits.max_bundle_bytes() {
        return Err(Error::TooLarge {
            limit: limits.max_bundle_bytes(),
        });
    }
    if bundle.owner_state_chain.len() > VerificationLimits::MAX_TRANSITIONS {
        return Err(Error::TooLarge {
            limit: VerificationLimits::MAX_TRANSITIONS,
        });
    }
    let mut state = verify_owner_root(
        bundle
            .owner_root
            .as_ref()
            .ok_or_else(|| Error::Invalid("authorization bundle has no owner root".to_owned()))?,
    )?;
    for transition in &bundle.owner_state_chain {
        state = apply_transition(&state, transition, now_unix_seconds, limits)?;
    }
    let capability =
        verify_capability_chain(&state, &bundle.capability_chain, now_unix_seconds, limits)?;
    verify_subject_biscuit(capability.capability(), &bundle.subject_biscuit)?;
    Ok(VerifiedAuthorizationBundle {
        owner_state: state,
        capability,
    })
}
