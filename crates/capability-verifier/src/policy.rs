// SPDX-License-Identifier: MIT OR Apache-2.0
//! Complete offline verification of owner-signed Spool policy chains.
//!
//! Verifies client-signed `SignedSpoolPolicyRecord` values. Hosts never
//! construct or sign a record. Canonical encoding is SHA-256 domain
//! separated and never protobuf serialization.

use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use heddle_api::heddle::api::v1alpha2::{
    Audience, AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey,
    SignedPolicyBody, SignedPolicyHead, SignedPolicyMergeRule, SignedPolicyMergeSemantics,
    SignedSpoolPolicy, SignedSpoolPolicyRecord,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

const POLICY_STATE_DOMAIN: &[u8] = b"heddle-spool-signed-policy-v2";
const POLICY_SIGNATURE_DOMAIN: &[u8] = b"heddle-spool-signed-policy-signature-v2";
const OWNER_KEY_DOMAIN: &[u8] = b"heddle-key-v1";
const FORMAT_VERSION: u32 = 1;

/// Canonical field names on `SignedSpoolPolicy`. Unknown / missing / duplicate
/// merge-policy keys fail closed.
pub const POLICY_SETTING_KEYS: [&str; 2] = ["max_audience", "revoked_key_ids"];

/// Grow-only union. A successor cannot subtract a revoked key id.
pub const GROW_ONLY_SETTING_KEYS: [&str; 1] = ["revoked_key_ids"];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
/// A typed rejection of owner-signed policy evidence.
pub enum OwnerGovernanceError {
    /// signed spool policy is invalid: details.
    #[error("signed spool policy is invalid: {0}")]
    Invalid(String),
    /// signed spool policy chain is broken: details.
    #[error("signed spool policy chain is broken: {0}")]
    BrokenChain(String),
    /// signed spool policy sequence {submitted} rolls back accepted sequence {accepted}.
    #[error("signed spool policy sequence {submitted} rolls back accepted sequence {accepted}")]
    Rollback {
        /// Submitted sequence.
        submitted: u64,
        /// Already accepted sequence.
        accepted: u64,
    },
    /// signed spool policy is not signed by the owner authority.
    #[error("signed spool policy is not signed by the owner authority")]
    NotOwnerSigned,
    /// signed spool policy merge would drop a grow-only entry.
    #[error("signed spool policy merge would drop a grow-only entry")]
    GrowOnlyDropped,
    /// signed spool policy max_audience exceeds inherited owner-signed ceiling.
    #[error("signed spool policy max_audience exceeds inherited owner-signed ceiling")]
    AncestorCeiling,
    /// signed spool policy transfer sequence is back-dated.
    #[error("signed spool policy transfer sequence is back-dated")]
    BackdatedK,
    /// signed spool policy introduces a revocation of an owner-history authority key.
    #[error("signed spool policy revokes its own authority key")]
    SelfRevocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// The verified tip and accumulated grow-only policy state.
pub struct VerifiedSignedPolicy {
    /// Spool uuid.
    pub spool_uuid: [u8; 16],
    /// Sequence.
    pub sequence: u64,
    /// Policy state hash.
    pub policy_state_hash: [u8; 32],
    /// Owner id.
    pub owner_id: [u8; 32],
    /// Owner state hash.
    pub owner_state_hash: [u8; 32],
    /// Ownership transfer sequence.
    pub ownership_transfer_sequence: u64,
    /// Policy.
    pub policy: SignedSpoolPolicy,
    /// Grow only.
    pub grow_only: BTreeMap<String, BTreeSet<Vec<u8>>>,
}

fn length_prefixed(value: &[u8], output: &mut Vec<u8>) -> Result<(), OwnerGovernanceError> {
    let len = u32::try_from(value.len()).map_err(|_| {
        OwnerGovernanceError::Invalid("signed policy field is too large".to_string())
    })?;
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}

fn canonical_policy(
    policy: &SignedSpoolPolicy,
    output: &mut Vec<u8>,
) -> Result<(), OwnerGovernanceError> {
    let mut ids: Vec<&[u8]> = policy.revoked_key_ids.iter().map(Vec::as_slice).collect();
    ids.sort();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(OwnerGovernanceError::Invalid(
            "revoked_key_ids must be unique".to_string(),
        ));
    }
    if ids.iter().any(|id| id.len() != 32) {
        return Err(OwnerGovernanceError::Invalid(
            "revoked_key_ids entries must be 32 bytes".to_string(),
        ));
    }
    if ids
        != policy
            .revoked_key_ids
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>()
    {
        return Err(OwnerGovernanceError::Invalid(
            "revoked_key_ids must be sorted bytewise".to_string(),
        ));
    }
    let count = u32::try_from(ids.len()).map_err(|_| {
        OwnerGovernanceError::Invalid("revoked_key_ids count does not fit u32".to_string())
    })?;
    output.extend_from_slice(&count.to_be_bytes());
    for id in ids {
        length_prefixed(id, output)?;
    }
    match policy.max_audience {
        None => output.push(0x00),
        Some(audience) => {
            output.push(0x01);
            output.extend_from_slice(&(audience as u32).to_be_bytes());
        }
    }
    Ok(())
}

/// Canonical body matching Decision 3(a). Fields 1–10, then optionally field 11.
pub fn canonical_signed_policy_body(
    body: &SignedPolicyBody,
    include_state_hash: bool,
) -> Result<Vec<u8>, OwnerGovernanceError> {
    let mut output = Vec::new();
    output.extend_from_slice(&body.format_version.to_be_bytes());
    length_prefixed(&body.spool_uuid, &mut output)?;

    let expected_head = body.expected_head.clone().unwrap_or_else(zero_head);
    length_prefixed(&expected_head.state_hash, &mut output)?;
    output.extend_from_slice(&expected_head.sequence.to_be_bytes());
    output.extend_from_slice(&body.sequence.to_be_bytes());

    if !body.merge_parent_state_hashes.is_empty() {
        return Err(OwnerGovernanceError::Invalid(
            "format 1 merge_parent_state_hashes must be empty".to_string(),
        ));
    }
    output.extend_from_slice(&0u32.to_be_bytes());

    let policy = body.policy.as_ref().ok_or_else(|| {
        OwnerGovernanceError::Invalid("signed policy body is missing policy".to_string())
    })?;
    canonical_policy(policy, &mut output)?;

    let policy_count = u32::try_from(body.merge_policies.len()).map_err(|_| {
        OwnerGovernanceError::Invalid("merge policy count does not fit u32".to_string())
    })?;
    output.extend_from_slice(&policy_count.to_be_bytes());
    for rule in &body.merge_policies {
        length_prefixed(rule.setting_key.as_bytes(), &mut output)?;
        output.extend_from_slice(&rule.semantics.to_be_bytes());
    }

    length_prefixed(&body.owner_id, &mut output)?;
    length_prefixed(&body.owner_state_hash, &mut output)?;
    output.extend_from_slice(&body.ownership_transfer_sequence.to_be_bytes());
    if include_state_hash {
        length_prefixed(&body.policy_state_hash, &mut output)?;
    }
    Ok(output)
}

/// Hash the canonical policy body without its claimed state hash.
pub fn policy_state_hash(body: &SignedPolicyBody) -> Result<[u8; 32], OwnerGovernanceError> {
    let canonical = canonical_signed_policy_body(body, false)?;
    Ok(domain_hash(POLICY_STATE_DOMAIN, &canonical))
}

/// Digest signed policy fields including the claimed state hash.
pub fn policy_signature_digest(body: &SignedPolicyBody) -> Result<[u8; 32], OwnerGovernanceError> {
    let canonical = canonical_signed_policy_body(body, true)?;
    Ok(domain_hash(POLICY_SIGNATURE_DOMAIN, &canonical))
}

/// Compute the owner-key namespace id for an authority key.
pub fn owner_key_id(key: &AuthorizationVerificationKey) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(OWNER_KEY_DOMAIN);
    hasher.update(key.algorithm.to_be_bytes());
    hasher.update(&key.public_key);
    hasher.finalize().into()
}

fn domain_hash(domain: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(body);
    hasher.finalize().into()
}

/// The empty policy-chain predecessor.
pub fn zero_head() -> SignedPolicyHead {
    SignedPolicyHead {
        state_hash: vec![0; 32],
        sequence: 0,
    }
}

fn as_hash32(bytes: &[u8], what: &str) -> Result<[u8; 32], OwnerGovernanceError> {
    bytes
        .try_into()
        .map_err(|_| OwnerGovernanceError::Invalid(format!("{what} must be 32 bytes")))
}

fn as_uuid16(bytes: &[u8]) -> Result<[u8; 16], OwnerGovernanceError> {
    bytes
        .try_into()
        .map_err(|_| OwnerGovernanceError::Invalid("spool_uuid must be 16 bytes".to_string()))
}

/// Required format-1 semantics for a known policy setting.
pub fn required_merge_semantics(setting_key: &str) -> Option<SignedPolicyMergeSemantics> {
    if setting_key == "revoked_key_ids" {
        return Some(SignedPolicyMergeSemantics::GrowOnlySetUnion);
    }
    if setting_key == "max_audience" {
        return Some(SignedPolicyMergeSemantics::LastWriterWins);
    }
    None
}

/// Required sorted format-1 policy merge rules.
pub fn required_merge_policies() -> Vec<SignedPolicyMergeRule> {
    vec![
        SignedPolicyMergeRule {
            setting_key: "max_audience".to_string(),
            semantics: SignedPolicyMergeSemantics::LastWriterWins as i32,
        },
        SignedPolicyMergeRule {
            setting_key: "revoked_key_ids".to_string(),
            semantics: SignedPolicyMergeSemantics::GrowOnlySetUnion as i32,
        },
    ]
}

fn validate_merge_policies(policies: &[SignedPolicyMergeRule]) -> Result<(), OwnerGovernanceError> {
    let mut seen = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for policy in policies {
        if previous.is_some_and(|prev| policy.setting_key.as_str() <= prev) {
            return Err(OwnerGovernanceError::Invalid(
                "merge_policies must be sorted by setting_key".to_string(),
            ));
        }
        previous = Some(policy.setting_key.as_str());
        if !seen.insert(policy.setting_key.as_str()) {
            return Err(OwnerGovernanceError::Invalid(format!(
                "duplicate merge policy for {}",
                policy.setting_key
            )));
        }
        let Some(required) = required_merge_semantics(&policy.setting_key) else {
            return Err(OwnerGovernanceError::Invalid(format!(
                "unknown signed-policy setting key {}",
                policy.setting_key
            )));
        };
        if policy.semantics != required as i32 {
            if required == SignedPolicyMergeSemantics::GrowOnlySetUnion {
                return Err(OwnerGovernanceError::Invalid(format!(
                    "cannot downgrade grow-only key {}",
                    policy.setting_key
                )));
            }
            return Err(OwnerGovernanceError::Invalid(format!(
                "merge semantics for {} must be {:?}",
                policy.setting_key, required
            )));
        }
    }
    for key in POLICY_SETTING_KEYS {
        if !seen.contains(key) {
            return Err(OwnerGovernanceError::Invalid(format!(
                "missing merge policy for {key}"
            )));
        }
    }
    Ok(())
}

/// Merge parent sets, refusing any proposal that drops inherited entries.
pub fn merge_grow_only_sets(
    parents: &[&BTreeMap<String, BTreeSet<Vec<u8>>>],
    proposed: &BTreeMap<String, BTreeSet<Vec<u8>>>,
    grow_only_keys: &[&str],
) -> Result<BTreeMap<String, BTreeSet<Vec<u8>>>, OwnerGovernanceError> {
    let mut merged = BTreeMap::new();
    for key in grow_only_keys {
        let mut union = BTreeSet::new();
        for parent in parents {
            if let Some(entries) = parent.get(*key) {
                union.extend(entries.iter().cloned());
            }
        }
        let proposed_entries = proposed.get(*key).cloned().unwrap_or_default();
        if !union.is_subset(&proposed_entries) {
            return Err(OwnerGovernanceError::GrowOnlyDropped);
        }
        union.extend(proposed_entries);
        if !union.is_empty() {
            merged.insert((*key).to_string(), union);
        }
    }
    Ok(merged)
}

fn verify_owner_signature(
    digest: &[u8; 32],
    signature: &AuthorizationSignature,
    key: &AuthorizationVerificationKey,
) -> Result<(), OwnerGovernanceError> {
    if key.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32 {
        return Err(OwnerGovernanceError::NotOwnerSigned);
    }
    if signature.signer_key_id.as_slice() != owner_key_id(key) {
        return Err(OwnerGovernanceError::NotOwnerSigned);
    }
    let public_key: &[u8; 32] = key
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
    let signature_bytes: &[u8; 64] = signature
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
    let verifying_key =
        VerifyingKey::from_bytes(public_key).map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
    verifying_key
        .verify(digest, &Signature::from_bytes(signature_bytes))
        .map_err(|_| OwnerGovernanceError::NotOwnerSigned)
}

/// Validate and collect proposed key revocations.
pub fn proposed_revocations(
    policy: &SignedSpoolPolicy,
) -> Result<BTreeSet<Vec<u8>>, OwnerGovernanceError> {
    if policy.revoked_key_ids.len() > 4096 {
        return Err(OwnerGovernanceError::Invalid(
            "revoked_key_ids count exceeds 4096".into(),
        ));
    }
    let mut set = BTreeSet::new();
    for id in &policy.revoked_key_ids {
        if id.len() != 32 {
            return Err(OwnerGovernanceError::Invalid(
                "revoked_key_ids entries must be 32 bytes".to_string(),
            ));
        }
        if !set.insert(id.clone()) {
            return Err(OwnerGovernanceError::Invalid(
                "revoked_key_ids must be unique".to_string(),
            ));
        }
    }
    Ok(set)
}

/// Present `UNSPECIFIED` fails closed. Absence is `None` (no own contribution).
pub fn parsed_max_audience(
    policy: &SignedSpoolPolicy,
) -> Result<Option<Audience>, OwnerGovernanceError> {
    match policy.max_audience {
        None => Ok(None),
        Some(value) => match Audience::try_from(value) {
            Ok(Audience::Unspecified) => Err(OwnerGovernanceError::Invalid(
                "present max_audience UNSPECIFIED fails closed".to_string(),
            )),
            Ok(audience) => Ok(Some(audience)),
            Err(_) => Err(OwnerGovernanceError::Invalid(
                "max_audience is not a known Audience discriminant".to_string(),
            )),
        },
    }
}

/// Trusted context for verifying one successor policy record.
pub struct VerifySignedPolicy<'a> {
    /// Signed.
    pub signed: &'a SignedSpoolPolicyRecord,
    /// Spool uuid.
    pub spool_uuid: [u8; 16],
    /// Accepted head.
    pub accepted_head: &'a SignedPolicyHead,
    /// Accepted owner id.
    pub accepted_owner_id: [u8; 32],
    /// Accepted owner state hash.
    pub accepted_owner_state_hash: [u8; 32],
    /// Required transfer sequence.
    pub required_transfer_sequence: u64,
    /// Authority key.
    pub authority_key: &'a AuthorizationVerificationKey,
    /// All authority key ids from the owner's verified accepted history.
    pub owner_authority_key_ids: &'a [[u8; 32]],
    /// Accepted grow only.
    pub accepted_grow_only: &'a BTreeMap<String, BTreeSet<Vec<u8>>>,
    /// Ancestor ceiling.
    pub ancestor_ceiling: Option<Audience>,
}

/// Verify a successor against selected predecessor and owner authority.
pub fn verify_signed_spool_policy_record(
    input: VerifySignedPolicy<'_>,
) -> Result<VerifiedSignedPolicy, OwnerGovernanceError> {
    let VerifySignedPolicy {
        signed,
        spool_uuid,
        accepted_head,
        accepted_owner_id,
        accepted_owner_state_hash,
        required_transfer_sequence,
        authority_key,
        owner_authority_key_ids,
        accepted_grow_only,
        ancestor_ceiling,
    } = input;
    let mut verified = verify_signed_spool_policy_record_integrity(signed, authority_key)?;
    if verified.spool_uuid != spool_uuid {
        return Err(OwnerGovernanceError::Invalid(
            "signed policy names another spool".to_string(),
        ));
    }
    let body = signed.body.as_ref().ok_or_else(|| {
        OwnerGovernanceError::Invalid("signed policy record is missing body".to_string())
    })?;
    let expected_head = body.expected_head.clone().unwrap_or_else(zero_head);
    as_hash32(&accepted_head.state_hash, "accepted_head.state_hash")?;
    if expected_head.sequence != accepted_head.sequence
        || expected_head.state_hash != accepted_head.state_hash
    {
        return Err(OwnerGovernanceError::BrokenChain(
            "expected_head does not match the accepted signed-policy head".to_string(),
        ));
    }
    if verified.owner_id != accepted_owner_id
        || verified.owner_state_hash != accepted_owner_state_hash
        || verified.ownership_transfer_sequence != required_transfer_sequence
    {
        return Err(OwnerGovernanceError::NotOwnerSigned);
    }
    let audience = parsed_max_audience(&verified.policy)?;
    if let (Some(proposed), Some(ceiling)) = (audience, ancestor_ceiling)
        && (proposed as i32) > (ceiling as i32)
    {
        return Err(OwnerGovernanceError::AncestorCeiling);
    }
    let authority_id = owner_key_id(authority_key);
    // F7 applies on introduction. Only authenticated predecessor state can
    // distinguish new revocations from those retained after an ownership change.
    if verified.policy.revoked_key_ids.iter().any(|id| {
        !accepted_grow_only
            .get("revoked_key_ids")
            .is_some_and(|set| set.contains(id))
            && (id.as_slice() == authority_id
                || owner_authority_key_ids
                    .iter()
                    .any(|key| id.as_slice() == key))
    }) {
        return Err(OwnerGovernanceError::SelfRevocation);
    }
    let mut parent_sets: Vec<&BTreeMap<String, BTreeSet<Vec<u8>>>> = Vec::new();
    if accepted_head.sequence > 0 {
        parent_sets.push(accepted_grow_only);
    }
    verified.grow_only =
        merge_grow_only_sets(&parent_sets, &verified.grow_only, &GROW_ONLY_SETTING_KEYS)?;
    Ok(verified)
}

/// Verify retained bytes and owner signature without admitting a successor.
/// The returned set is this record's contribution, never predecessor state.
pub(crate) fn verify_signed_spool_policy_record_integrity(
    signed: &SignedSpoolPolicyRecord,
    authority_key: &AuthorizationVerificationKey,
) -> Result<VerifiedSignedPolicy, OwnerGovernanceError> {
    let body = signed.body.as_ref().ok_or_else(|| {
        OwnerGovernanceError::Invalid("signed policy record is missing body".to_string())
    })?;
    if body.format_version != FORMAT_VERSION {
        return Err(OwnerGovernanceError::Invalid(
            "signed policy format_version must be 1".to_string(),
        ));
    }
    let submitted_spool = as_uuid16(&body.spool_uuid)?;

    let expected_head = body.expected_head.clone().unwrap_or_else(zero_head);
    as_hash32(&expected_head.state_hash, "expected_head.state_hash")?;
    if body.sequence <= expected_head.sequence {
        return Err(OwnerGovernanceError::Rollback {
            submitted: body.sequence,
            accepted: expected_head.sequence,
        });
    }
    if body.sequence != expected_head.sequence.saturating_add(1) {
        return Err(OwnerGovernanceError::BrokenChain(
            "signed policy sequence must be exactly accepted_head.sequence + 1".to_string(),
        ));
    }
    if !body.merge_parent_state_hashes.is_empty() {
        return Err(OwnerGovernanceError::Invalid(
            "format 1 merge_parent_state_hashes must be empty".to_string(),
        ));
    }

    validate_merge_policies(&body.merge_policies)?;

    let recomputed = policy_state_hash(body)?;
    let claimed = as_hash32(&body.policy_state_hash, "policy_state_hash")?;
    if recomputed != claimed {
        return Err(OwnerGovernanceError::BrokenChain(
            "policy_state_hash does not match the canonical body".to_string(),
        ));
    }

    let owner_id = as_hash32(&body.owner_id, "owner_id")?;
    let owner_state_hash = as_hash32(&body.owner_state_hash, "owner_state_hash")?;

    let signature = signed
        .owner_signature
        .as_ref()
        .ok_or(OwnerGovernanceError::NotOwnerSigned)?;
    let digest = policy_signature_digest(body)?;
    verify_owner_signature(&digest, signature, authority_key)?;

    let policy = body.policy.clone().ok_or_else(|| {
        OwnerGovernanceError::Invalid("signed policy body is missing policy".to_string())
    })?;
    parsed_max_audience(&policy)?;
    let proposed = proposed_revocations(&policy)?;
    let mut grow_only = BTreeMap::new();
    if !proposed.is_empty() {
        grow_only.insert("revoked_key_ids".to_string(), proposed);
    }

    Ok(VerifiedSignedPolicy {
        spool_uuid: submitted_spool,
        sequence: body.sequence,
        policy_state_hash: claimed,
        owner_id,
        owner_state_hash,
        ownership_transfer_sequence: body.ownership_transfer_sequence,
        policy,
        grow_only,
    })
}

/// Offline replay from the empty head. `k_max` is the verified keyring's last
/// transfer sequence (0 if none). A chain whose tip has `k < k_max` is refused
/// (N2 / monotonic pin).
pub fn verify_signed_policy_chain<F>(
    chain: &[SignedSpoolPolicyRecord],
    spool_uuid: &[u8; 16],
    k_max: u64,
    mut resolve_owner: F,
) -> Result<VerifiedSignedPolicy, OwnerGovernanceError>
where
    F: FnMut(
        &[u8; 32],
        &[u8; 32],
        u64,
    ) -> Result<(AuthorizationVerificationKey, Vec<[u8; 32]>), OwnerGovernanceError>,
{
    if chain.is_empty() {
        return Err(OwnerGovernanceError::Invalid(
            "signed policy chain is empty".to_string(),
        ));
    }
    let mut accepted_head = zero_head();
    let mut accepted_grow_only = BTreeMap::new();
    let mut last_k = 0u64;
    let mut saw_k_max = k_max == 0;
    let mut tip = None;
    for signed in chain {
        let body = signed.body.as_ref().ok_or_else(|| {
            OwnerGovernanceError::Invalid("signed policy record is missing body".to_string())
        })?;
        let owner_id = as_hash32(&body.owner_id, "owner_id")?;
        let owner_state_hash = as_hash32(&body.owner_state_hash, "owner_state_hash")?;
        let k = body.ownership_transfer_sequence;
        if k < last_k {
            return Err(OwnerGovernanceError::BackdatedK);
        }
        last_k = k;
        if k == k_max {
            saw_k_max = true;
        }
        let (authority_key, owner_authority_key_ids) =
            resolve_owner(&owner_id, &owner_state_hash, k)?;
        let verified = verify_signed_spool_policy_record(VerifySignedPolicy {
            signed,
            spool_uuid: *spool_uuid,
            accepted_head: &accepted_head,
            accepted_owner_id: owner_id,
            accepted_owner_state_hash: owner_state_hash,
            required_transfer_sequence: k,
            authority_key: &authority_key,
            owner_authority_key_ids: &owner_authority_key_ids,
            accepted_grow_only: &accepted_grow_only,
            ancestor_ceiling: None,
        })?;
        accepted_head = SignedPolicyHead {
            state_hash: verified.policy_state_hash.to_vec(),
            sequence: verified.sequence,
        };
        accepted_grow_only = verified.grow_only.clone();
        tip = Some(verified);
    }
    let tip = tip
        .ok_or_else(|| OwnerGovernanceError::Invalid("signed policy chain is empty".to_string()))?;
    if !saw_k_max || tip.ownership_transfer_sequence != k_max {
        return Err(OwnerGovernanceError::BackdatedK);
    }
    Ok(tip)
}

/// Replay policy against independently verified resource ownership and current
/// account state. Each record's transfer sequence selects its sole owning
/// account; its exact state hash selects a verified historical authority key.
pub fn verify_resource_policy_chain(
    chain: &[SignedSpoolPolicyRecord],
    keyring: &crate::VerifiedCloneKeyring,
    current: &crate::VerifiedOwnerState,
    now: i64,
    limits: crate::VerificationLimits,
) -> crate::Result<VerifiedSignedPolicy> {
    use crate::canonical::fixed;
    keyring.verify_current_owner(current, now, limits)?;
    let mut owners = vec![keyring.owner_state().clone(), current.clone()];
    for history in &keyring.wire().transfer_owner_histories {
        owners.push(crate::observed::verify_history(history, now, limits)?);
    }
    let initial_uuid = keyring
        .owner_state()
        .signed_root()
        .root
        .as_ref()
        .ok_or_else(|| crate::Error::BrokenChain("verified owner has no root".into()))?
        .account_uuid
        .as_slice();
    let audits = &keyring.wire().ownership_transfers;
    let k_max = u64::try_from(audits.len())
        .map_err(|_| crate::Error::Invalid("transfer count overflow".into()))?;
    Ok(verify_signed_policy_chain(
        chain,
        &fixed(&keyring.wire().spool_uuid, "Spool UUID")?,
        k_max,
        |id, hash, k| {
            let index = usize::try_from(k).map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
            if k > k_max {
                return Err(OwnerGovernanceError::NotOwnerSigned);
            }
            let handoff_at = |index: usize| {
                audits
                    .get(index)
                    .and_then(|a| a.transfer.as_ref())
                    .and_then(|t| t.acceptance.as_ref())
                    .and_then(|a| a.signed_handoff.as_ref())
                    .and_then(|s| s.handoff.as_ref())
                    .ok_or(OwnerGovernanceError::NotOwnerSigned)
            };
            let entry = if index == 0 {
                None
            } else {
                Some(handoff_at(index - 1)?)
            };
            let uuid = entry.map_or(initial_uuid, |h| h.destination_owner_uuid.as_slice());
            // A previous owner's full account history can continue after it
            // surrendered this Spool. Policies in that phase cannot use those
            // later states. Likewise, a new owner's pre-handoff authority does
            // not become a valid policy signer for its new ownership phase.
            let upper_hash = if k == k_max {
                current.state_hash().to_vec()
            } else {
                handoff_at(index)?.source_owner_key_state_hash.clone()
            };
            let owner = owners
                .iter()
                .find(|o| {
                    o.owner_id() == *id
                        && o.state_hash().as_slice() == upper_hash
                        && o.signed_root()
                            .root
                            .as_ref()
                            .is_some_and(|r| r.account_uuid == uuid)
                })
                .ok_or(OwnerGovernanceError::NotOwnerSigned)?;
            let sequence = owner
                .issuers_sequence(hash)
                .map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
            if let Some(entry) = entry {
                let floor = owner
                    .issuers_sequence(&entry.destination_owner_key_state_hash)
                    .map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
                if sequence < floor {
                    return Err(OwnerGovernanceError::NotOwnerSigned);
                }
            }
            let authority = owner
                .provenance_issuer(hash, sequence)
                .cloned()
                .map_err(|_| OwnerGovernanceError::NotOwnerSigned)?;
            // Signing is phase-bounded, but F7 excludes every accepted key of
            // this owner, including rotations after it surrendered the Spool.
            let ids = owners
                .iter()
                .filter(|o| o.owner_id() == *id)
                .flat_map(crate::VerifiedOwnerState::authority_key_ids)
                .collect();
            Ok((authority, ids))
        },
    )?)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use prost::Message;

    use super::*;

    fn authority() -> (SigningKey, AuthorizationVerificationKey) {
        let signing = SigningKey::from_bytes(&[0x58; 32]);
        let key = AuthorizationVerificationKey {
            algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
            public_key: signing.verifying_key().as_bytes().to_vec(),
        };
        (signing, key)
    }

    fn sign(
        mut body: SignedPolicyBody,
        signing: &SigningKey,
        key: &AuthorizationVerificationKey,
    ) -> SignedSpoolPolicyRecord {
        body.policy_state_hash = policy_state_hash(&body).expect("hash").to_vec();
        let digest = policy_signature_digest(&body).expect("digest");
        let signature = signing.sign(&digest);
        SignedSpoolPolicyRecord {
            body: Some(body),
            owner_signature: Some(AuthorizationSignature {
                signer_key_id: owner_key_id(key).to_vec(),
                signature: signature.to_bytes().to_vec(),
            }),
        }
    }

    fn owner_id() -> [u8; 32] {
        [0x42; 32]
    }

    fn owner_hash() -> [u8; 32] {
        [0x11; 32]
    }

    fn base_body() -> SignedPolicyBody {
        SignedPolicyBody {
            format_version: 1,
            spool_uuid: vec![0x11; 16],
            expected_head: Some(zero_head()),
            sequence: 1,
            merge_parent_state_hashes: Vec::new(),
            policy: Some(SignedSpoolPolicy {
                revoked_key_ids: Vec::new(),
                max_audience: Some(Audience::Private as i32),
            }),
            merge_policies: required_merge_policies(),
            owner_id: owner_id().to_vec(),
            owner_state_hash: owner_hash().to_vec(),
            ownership_transfer_sequence: 0,
            policy_state_hash: Vec::new(),
        }
    }

    fn verify(
        signed: &SignedSpoolPolicyRecord,
        key: &AuthorizationVerificationKey,
    ) -> Result<VerifiedSignedPolicy, OwnerGovernanceError> {
        verify_signed_spool_policy_record(VerifySignedPolicy {
            signed,
            spool_uuid: [0x11; 16],
            accepted_head: &zero_head(),
            accepted_owner_id: owner_id(),
            accepted_owner_state_hash: owner_hash(),
            required_transfer_sequence: 0,
            authority_key: key,
            owner_authority_key_ids: &[owner_key_id(key)],
            accepted_grow_only: &BTreeMap::new(),
            ancestor_ceiling: None,
        })
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn genesis_private_ceiling_verifies() {
        let (signing, key) = authority();
        let signed = sign(base_body(), &signing, &key);
        let verified = verify(&signed, &key).expect("verify");
        assert_eq!(verified.sequence, 1);
        assert_eq!(verified.policy.max_audience, Some(Audience::Private as i32));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn present_unspecified_audience_fails_closed() {
        let (signing, key) = authority();
        let mut body = base_body();
        body.policy = Some(SignedSpoolPolicy {
            revoked_key_ids: Vec::new(),
            max_audience: Some(Audience::Unspecified as i32),
        });
        let signed = sign(body, &signing, &key);
        assert!(matches!(
            verify(&signed, &key),
            Err(OwnerGovernanceError::Invalid(_))
        ));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn absent_audience_is_no_own_contribution() {
        let (signing, key) = authority();
        let mut body = base_body();
        body.policy = Some(SignedSpoolPolicy {
            revoked_key_ids: Vec::new(),
            max_audience: None,
        });
        let signed = sign(body, &signing, &key);
        let verified = verify(&signed, &key).expect("verify");
        assert!(verified.policy.max_audience.is_none());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ancestor_ceiling_rejects_looser_child_cap() {
        let (signing, key) = authority();
        let mut body = base_body();
        body.policy = Some(SignedSpoolPolicy {
            revoked_key_ids: Vec::new(),
            max_audience: Some(Audience::Public as i32),
        });
        let signed = sign(body, &signing, &key);
        let error = verify_signed_spool_policy_record(VerifySignedPolicy {
            signed: &signed,
            spool_uuid: [0x11; 16],
            accepted_head: &zero_head(),
            accepted_owner_id: owner_id(),
            accepted_owner_state_hash: owner_hash(),
            required_transfer_sequence: 0,
            authority_key: &key,
            owner_authority_key_ids: &[owner_key_id(&key)],
            accepted_grow_only: &BTreeMap::new(),
            ancestor_ceiling: Some(Audience::Private),
        })
        .expect_err("ceiling");
        assert_eq!(error, OwnerGovernanceError::AncestorCeiling);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn grow_only_cannot_drop_a_revocation() {
        let (signing, key) = authority();
        let kept = vec![0x22; 32];
        let mut first = base_body();
        first.policy = Some(SignedSpoolPolicy {
            revoked_key_ids: vec![kept.clone()],
            max_audience: Some(Audience::Private as i32),
        });
        let signed_first = sign(first, &signing, &key);
        let verified = verify(&signed_first, &key).expect("first");

        let mut second = base_body();
        second.expected_head = Some(SignedPolicyHead {
            state_hash: verified.policy_state_hash.to_vec(),
            sequence: 1,
        });
        second.sequence = 2;
        second.policy = Some(SignedSpoolPolicy {
            revoked_key_ids: Vec::new(),
            max_audience: Some(Audience::Private as i32),
        });
        let signed_second = sign(second, &signing, &key);
        let error = verify_signed_spool_policy_record(VerifySignedPolicy {
            signed: &signed_second,
            spool_uuid: [0x11; 16],
            accepted_head: &SignedPolicyHead {
                state_hash: verified.policy_state_hash.to_vec(),
                sequence: 1,
            },
            accepted_owner_id: owner_id(),
            accepted_owner_state_hash: owner_hash(),
            required_transfer_sequence: 0,
            authority_key: &key,
            owner_authority_key_ids: &[owner_key_id(&key)],
            accepted_grow_only: &verified.grow_only,
            ancestor_ceiling: None,
        })
        .expect_err("drop");
        assert_eq!(error, OwnerGovernanceError::GrowOnlyDropped);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn introducing_self_revocation_is_refused() {
        let (signing, key) = authority();
        let mut body = base_body();
        body.policy = Some(SignedSpoolPolicy {
            revoked_key_ids: vec![owner_key_id(&key).to_vec()],
            max_audience: Some(Audience::Private as i32),
        });
        let signed = sign(body, &signing, &key);
        assert_eq!(
            verify(&signed, &key),
            Err(OwnerGovernanceError::SelfRevocation)
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn back_dated_k_tip_is_refused() {
        let (signing, key) = authority();
        let signed = sign(base_body(), &signing, &key);
        let error = verify_signed_policy_chain(&[signed], &[0x11; 16], 1, |_, _, _| {
            Ok((key.clone(), vec![owner_key_id(&key)]))
        })
        .expect_err("n2");
        assert_eq!(error, OwnerGovernanceError::BackdatedK);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn protobuf_bytes_are_not_the_signed_preimage() {
        let body = base_body();
        let canonical = canonical_signed_policy_body(&body, false).expect("canonical");
        assert_ne!(canonical, Message::encode_to_vec(&body));
    }
}
