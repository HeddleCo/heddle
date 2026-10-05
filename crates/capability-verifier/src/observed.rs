// SPDX-License-Identifier: MIT OR Apache-2.0
//! Production byte APIs for evidence returned by ObserveOwnership and SpoolEvent.
//!
//! Summaries use hex for bytes and decimal strings for sequences, preserving
//! every bit across native JSON and JavaScript. Verification establishes proof
//! integrity; callers still select and retain their own lineage and head pins.

use prost::Message;
use serde::Serialize;

use crate::{
    Error, Result, TransferOwner, VerificationLimits, VerifiedCloneKeyring, VerifiedOwnerState,
    canonical::{canonical_message, fixed},
    policy::{self, OwnerGovernanceError},
    wire::*,
};

/// Verified account authority, including its exact accepted state hash.
#[derive(Debug, Serialize)]
pub struct OwnerSummary {
    /// Account routing UUID.
    pub owner_uuid_hex: String,
    /// Cryptographic root identity.
    pub owner_id_hex: String,
    /// Exact accepted owner state hash.
    pub state_hash_hex: String,
    /// Lossless accepted owner-key sequence.
    pub sequence: String,
}

/// Verified two-party resource handoff.
#[derive(Debug, Serialize)]
pub struct TransferSummary {
    /// Immutable resource UUID.
    pub resource_uuid_hex: String,
    /// Gap-free ownership-transfer sequence.
    pub transfer_sequence: String,
    /// Source account UUID.
    pub source_owner_uuid_hex: String,
    /// Exact source signing state.
    pub source_state_hash_hex: String,
    /// Destination account UUID.
    pub destination_owner_uuid_hex: String,
    /// Exact destination accepting state.
    pub destination_state_hash_hex: String,
}

/// Complete verified resource lineage and current account authority.
#[derive(Debug, Serialize)]
pub struct ResourceKeyringSummary {
    /// Immutable resource UUID.
    pub spool_uuid_hex: String,
    /// Immutable genesis digest for independent pinning.
    pub spool_genesis_digest_hex: String,
    /// Current sole owner, including rotations after the final transfer.
    pub current_owner: OwnerSummary,
    /// Accepted transfer sequence (zero for the genesis owner).
    pub accepted_transfer_sequence: String,
    /// Verified handoffs in accepted order.
    pub ownership_transfers: Vec<TransferSummary>,
    /// Verified audit hashes in the same order.
    pub audit_record_hashes_hex: Vec<String>,
}

/// Verified self-signed or delegated-creation genesis.
#[derive(Debug, Serialize)]
pub struct GenesisSummary {
    /// Immutable Spool UUID.
    pub spool_uuid_hex: String,
    /// Immutable owner-record namespace digest.
    pub spool_genesis_digest_hex: String,
    /// Genesis owner authority key id.
    pub owner_key_id_hex: String,
    /// Whether a complete SpoolCreationProof authenticated this genesis.
    pub delegated_creation: bool,
}

/// Verified policy-chain tip and grow-only revocations.
#[derive(Debug, Serialize)]
pub struct PolicySummary {
    /// Immutable Spool UUID.
    pub spool_uuid_hex: String,
    /// Lossless policy sequence.
    pub sequence: String,
    /// Exact verified policy head hash.
    pub policy_state_hash_hex: String,
    /// Cryptographic identity of the signing owner.
    pub owner_id_hex: String,
    /// Exact authority state used to sign the tip.
    pub owner_state_hash_hex: String,
    /// Accepted ownership-transfer sequence of the tip.
    pub ownership_transfer_sequence: String,
    /// Verified sorted grow-only revocations.
    pub revoked_key_ids_hex: Vec<String>,
    /// Optional Audience discriminant; absent contribution is null.
    pub max_audience: Option<i32>,
}

fn owner_uuid(state: &VerifiedOwnerState) -> Result<[u8; 16]> {
    fixed(
        &state
            .signed_root()
            .root
            .as_ref()
            .ok_or_else(|| Error::BrokenChain("verified root has no body".into()))?
            .account_uuid,
        "owner account UUID",
    )
}

pub(crate) fn owner_summary(state: &VerifiedOwnerState) -> Result<OwnerSummary> {
    Ok(OwnerSummary {
        owner_uuid_hex: hex::encode(owner_uuid(state)?),
        owner_id_hex: hex::encode(state.owner_id()),
        state_hash_hex: hex::encode(state.state_hash()),
        sequence: state.sequence().to_string(),
    })
}

/// Verify an observed SignedOwnerRoot and return its exact authority state.
pub fn verify_owner_root_bytes(root: &[u8]) -> Result<OwnerSummary> {
    owner_summary(&crate::verify_owner_root(&canonical_message(
        root,
        64 * 1024,
    )?)?)
}

pub(crate) fn verify_history(
    history: &OwnerHistory,
    now: i64,
    limits: VerificationLimits,
) -> Result<VerifiedOwnerState> {
    if history.encoded_len() > limits.max_bundle_bytes()
        || history.accepted_transitions.len() > VerificationLimits::MAX_TRANSITIONS
    {
        return Err(Error::TooLarge {
            limit: limits.max_bundle_bytes(),
        });
    }
    let mut state = crate::verify_owner_root(
        history
            .root
            .as_ref()
            .ok_or_else(|| Error::BrokenChain("owner history has no root".into()))?,
    )?;
    for transition in &history.accepted_transitions {
        state = crate::apply_accepted_transition(&state, transition, now, limits)?;
    }
    if history.state_hash.as_slice() != state.state_hash() {
        return Err(Error::BrokenChain(
            "owner history hash differs from its signed proof".into(),
        ));
    }
    Ok(state)
}

pub(crate) fn ownership(
    keyring: &[u8],
    current_owner: &[u8],
    now: i64,
    limits: VerificationLimits,
) -> Result<(VerifiedCloneKeyring, VerifiedOwnerState)> {
    let keyring = crate::verify_clone_keyring_bytes(keyring, now, limits, &[])?;
    let observed: OwnerState = canonical_message(current_owner, limits.max_bundle_bytes())?;
    let current = verify_history(
        &OwnerHistory {
            root: observed.root,
            accepted_transitions: observed.accepted_transitions,
            state_hash: observed.version,
        },
        now,
        limits,
    )?;
    keyring.verify_current_owner(&current, now, limits)?;
    Ok((keyring, current))
}

fn handoff(transfer: &ResourceOwnershipTransfer) -> Result<&ResourceTransferHandoff> {
    transfer
        .acceptance
        .as_ref()
        .and_then(|v| v.signed_handoff.as_ref())
        .and_then(|v| v.handoff.as_ref())
        .ok_or_else(|| Error::BrokenChain("transfer has no complete handoff".into()))
}

fn transfer_summary(transfer: &ResourceOwnershipTransfer) -> Result<TransferSummary> {
    let body = handoff(transfer)?;
    Ok(TransferSummary {
        resource_uuid_hex: hex::encode(&body.resource_uuid),
        transfer_sequence: body.transfer_sequence.to_string(),
        source_owner_uuid_hex: hex::encode(&body.source_owner_uuid),
        source_state_hash_hex: hex::encode(&body.source_owner_key_state_hash),
        destination_owner_uuid_hex: hex::encode(&body.destination_owner_uuid),
        destination_state_hash_hex: hex::encode(&body.destination_owner_key_state_hash),
    })
}

/// Verify a ResourceOwnershipTransfer and both exact OwnerHistory witnesses.
#[allow(clippy::too_many_arguments)]
pub fn verify_ownership_transfer_bytes(
    transfer: &[u8],
    source_history: &[u8],
    destination_history: &[u8],
    resource_uuid: &[u8],
    expected_sequence: u64,
    now: i64,
    max_ttl: i64,
) -> Result<TransferSummary> {
    let limits = VerificationLimits::new(max_ttl)?;
    let source = verify_history(
        &canonical_message(source_history, limits.max_bundle_bytes())?,
        now,
        limits,
    )?;
    let destination = verify_history(
        &canonical_message(destination_history, limits.max_bundle_bytes())?,
        now,
        limits,
    )?;
    let transfer = canonical_message(transfer, limits.max_bundle_bytes())?;
    crate::verify_resource_transfer(
        &transfer,
        &fixed(resource_uuid, "resource UUID")?,
        expected_sequence,
        TransferOwner {
            stable_owner_uuid: &owner_uuid(&source)?,
            state: &source,
        },
        TransferOwner {
            stable_owner_uuid: &owner_uuid(&destination)?,
            state: &destination,
        },
    )?;
    transfer_summary(&transfer)
}

/// Verify the exact CloneAuthorizationKeyring and OwnerState from ObserveOwnership.
pub fn verify_resource_keyring_bytes(
    keyring: &[u8],
    current_owner: &[u8],
    now: i64,
    max_ttl: i64,
) -> Result<ResourceKeyringSummary> {
    let (keyring, current) = ownership(
        keyring,
        current_owner,
        now,
        VerificationLimits::new(max_ttl)?,
    )?;
    let wire = keyring.wire();
    let ownership_transfers =
        wire.ownership_transfers
            .iter()
            .map(|record| {
                transfer_summary(
                    record.transfer.as_ref().ok_or_else(|| {
                        Error::BrokenChain("verified audit missing transfer".into())
                    })?,
                )
            })
            .collect::<Result<Vec<_>>>()?;
    let genesis = keyring
        .owner_genesis()
        .signed()
        .genesis
        .as_ref()
        .ok_or_else(|| Error::BrokenChain("verified genesis missing body".into()))?;
    Ok(ResourceKeyringSummary {
        spool_uuid_hex: hex::encode(&wire.spool_uuid),
        spool_genesis_digest_hex: hex::encode(crate::creation::spool_genesis_digest(genesis)?),
        current_owner: owner_summary(&current)?,
        accepted_transfer_sequence: ownership_transfers.len().to_string(),
        ownership_transfers,
        audit_record_hashes_hex: wire
            .ownership_transfers
            .iter()
            .map(|r| hex::encode(&r.audit_record_hash))
            .collect(),
    })
}

/// Verify observed immutable genesis, including delegated-creation signatures and history.
/// This proves historical structure; fresh creation admission is a separate API.
pub fn verify_spool_owner_genesis_bytes(genesis: &[u8], now: i64) -> Result<GenesisSummary> {
    let signed: SignedSpoolOwnerGenesis =
        canonical_message(genesis, crate::creation::MAX_CREATION_PROOF_BYTES + 1024)?;
    let verified = crate::verify_spool_owner_genesis(&signed)?;
    if signed.delegated_creation.is_some() {
        crate::creation::validate_spool_creation_structure(&signed, now)?;
    }
    Ok(GenesisSummary {
        spool_uuid_hex: hex::encode(verified.spool_uuid()),
        spool_genesis_digest_hex: hex::encode(crate::creation::spool_genesis_digest(
            signed
                .genesis
                .as_ref()
                .ok_or_else(|| Error::Invalid("genesis missing body".into()))?,
        )?),
        owner_key_id_hex: hex::encode(policy::owner_key_id(verified.owner_public_key())),
        delegated_creation: signed.delegated_creation.is_some(),
    })
}

/// Replay a complete observed policy chain with transfer-aware owner authority.
/// Policy records are the exact SpoolEvent.signed_policy protobuf bytes in order.
pub fn verify_signed_policy_chain_bytes(
    records: &[Vec<u8>],
    keyring: &[u8],
    current_owner: &[u8],
    now: i64,
    max_ttl: i64,
) -> Result<PolicySummary> {
    let limits = VerificationLimits::new(max_ttl)?;
    if records.len() > limits.max_bundle_bytes() {
        return Err(Error::TooLarge {
            limit: limits.max_bundle_bytes(),
        });
    }
    if records
        .iter()
        .try_fold(0_usize, |n, r| n.checked_add(r.len()))
        .is_none_or(|n| n > limits.max_bundle_bytes())
    {
        return Err(Error::TooLarge {
            limit: limits.max_bundle_bytes(),
        });
    }
    let (keyring, current) = ownership(keyring, current_owner, now, limits)?;
    let chain = records
        .iter()
        .map(|r| canonical_message(r, limits.max_bundle_bytes()))
        .collect::<Result<Vec<SignedSpoolPolicyRecord>>>()?;
    let tip = policy::verify_resource_policy_chain(&chain, &keyring, &current, now, limits)?;
    Ok(PolicySummary {
        spool_uuid_hex: hex::encode(tip.spool_uuid),
        sequence: tip.sequence.to_string(),
        policy_state_hash_hex: hex::encode(tip.policy_state_hash),
        owner_id_hex: hex::encode(tip.owner_id),
        owner_state_hash_hex: hex::encode(tip.owner_state_hash),
        ownership_transfer_sequence: tip.ownership_transfer_sequence.to_string(),
        revoked_key_ids_hex: tip.policy.revoked_key_ids.iter().map(hex::encode).collect(),
        max_audience: tip.policy.max_audience,
    })
}

/// Stable structured error for native/JavaScript callers; branch on code.
#[derive(Debug, Serialize)]
pub struct VerificationError {
    /// Typed failure category.
    pub code: VerificationErrorCode,
    /// Human-readable diagnostic, never a dispatch key.
    pub message: String,
    /// Exact preparation refusal detail; absent for other failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preparation_refusal_reason: Option<PreparationRefusalReason>,
}

impl From<Error> for VerificationError {
    fn from(error: Error) -> Self {
        use VerificationErrorCode as C;
        let code = match &error {
            Error::Invalid(_) => C::Invalid,
            Error::InvalidSignature => C::InvalidSignature,
            Error::BrokenChain(_) => C::BrokenChain,
            Error::RecoveryThreshold { .. } => C::RecoveryThreshold,
            Error::CapabilityDenied(_) => C::CapabilityDenied,
            Error::Expired => C::Expired,
            Error::NotYetValid => C::NotYetValid,
            Error::NonCanonicalProtobuf => C::NonCanonicalProtobuf,
            Error::TooLarge { .. } => C::TooLarge,
            Error::Decode(_) => C::Decode,
            Error::Biscuit(_) => C::Biscuit,
            Error::Policy(p) => match p {
                OwnerGovernanceError::Invalid(_) => C::PolicyInvalid,
                OwnerGovernanceError::BrokenChain(_) => C::PolicyBrokenChain,
                OwnerGovernanceError::Rollback { .. } => C::PolicyRollback,
                OwnerGovernanceError::NotOwnerSigned => C::PolicyNotOwnerSigned,
                OwnerGovernanceError::GrowOnlyDropped => C::PolicyGrowOnlyDropped,
                OwnerGovernanceError::AncestorCeiling => C::PolicyAncestorCeiling,
                OwnerGovernanceError::BackdatedK => C::PolicyBackdatedTransfer,
                OwnerGovernanceError::SelfRevocation => C::PolicySelfRevocation,
            },
            Error::Hybrid(reason) => hybrid_error_code(*reason),
        };
        let preparation_refusal_reason = match &error {
            Error::Hybrid(heddle_api::hybrid_codec::Reject::PreparationRefused(reason)) => {
                Some((*reason).into())
            }
            _ => None,
        };
        Self {
            code,
            message: error.to_string(),
            preparation_refusal_reason,
        }
    }
}

/// Exhaustive verification failure categories exposed to JavaScript.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationErrorCode {
    /// Invalid.
    Invalid,
    /// Invalid Signature.
    InvalidSignature,
    /// Broken Chain.
    BrokenChain,
    /// Recovery Threshold.
    RecoveryThreshold,
    /// Capability Denied.
    CapabilityDenied,
    /// Expired.
    Expired,
    /// Not Yet Valid.
    NotYetValid,
    /// Non Canonical Protobuf.
    NonCanonicalProtobuf,
    /// Too Large.
    TooLarge,
    /// Decode.
    Decode,
    /// Biscuit.
    Biscuit,
    /// Policy Invalid.
    PolicyInvalid,
    /// Policy Broken Chain.
    PolicyBrokenChain,
    /// Policy Rollback.
    PolicyRollback,
    /// Policy Not Owner Signed.
    PolicyNotOwnerSigned,
    /// Policy Grow Only Dropped.
    PolicyGrowOnlyDropped,
    /// Policy Ancestor Ceiling.
    PolicyAncestorCeiling,
    /// Policy Backdated Transfer.
    PolicyBackdatedTransfer,
    /// Policy Self Revocation.
    PolicySelfRevocation,
    /// Serialization.
    Serialization,
    /// Hybrid Version.
    HybridVersion,
    /// Hybrid Canonical.
    HybridCanonical,
    /// Hybrid Bounds.
    HybridBounds,
    /// Hybrid Signature.
    HybridSignature,
    /// Hybrid Root.
    HybridRoot,
    /// Hybrid Semantic.
    HybridSemantic,
    /// Hybrid High Water.
    HybridHighWater,
    /// Hybrid Transition.
    HybridTransition,
    /// Hybrid Job As Witness.
    HybridJobAsWitness,
    /// Hybrid Key Role.
    HybridKeyRole,
    /// The original native admission window ended.
    /// Hybrid Expired.
    HybridExpired,
    /// Hybrid Scope.
    HybridScope,
    /// Hybrid Prepared Fields.
    HybridPreparedFields,
    /// Preparation refused with a typed reason.
    HybridPreparationRefused,
    /// Observe ref disclosure is missing.
    HybridRefDisclosure,
    /// The selected commit is not pinned.
    HybridRefPinning,
    /// Provider source selection does not match.
    HybridSourceSelection,
    /// The source requires the Commit profile.
    HybridImportSourceRequiresCommit,
    /// The operation ID was reused with different bytes.
    HybridOperationIdReused,
    /// The created pending operation is absent.
    HybridPendingOperation,

    /// Hybrid Validity Bounds.
    HybridValidityBounds,
    /// Hybrid Genesis Binding.
    HybridGenesisBinding,
    /// Hybrid Import Permission.
    HybridImportPermission,
    /// Hybrid Stale Manifest.
    HybridStaleManifest,
    /// Hybrid Committed Slot.
    HybridCommittedSlot,
    /// Hybrid Stale Context.
    HybridStaleContext,
    /// Hybrid Proof.
    HybridProof,
    /// Hybrid Revoked.
    HybridRevoked,
    /// Hybrid Slot Conflict.
    HybridSlotConflict,
    /// Hybrid Boundary Acceptance.
    HybridBoundaryAcceptance,
    /// Hybrid Protocol.
    HybridProtocol,
}

fn hybrid_error_code(reason: heddle_api::hybrid_codec::Reject) -> VerificationErrorCode {
    use VerificationErrorCode as C;
    use heddle_api::hybrid_codec::Reject as R;
    match reason {
        R::Version => C::HybridVersion,
        R::Canonical => C::HybridCanonical,
        R::Bounds => C::HybridBounds,
        R::Signature => C::HybridSignature,
        R::Root => C::HybridRoot,
        R::Semantic => C::HybridSemantic,
        R::HighWater => C::HybridHighWater,
        R::Transition => C::HybridTransition,
        R::JobAsWitness => C::HybridJobAsWitness,
        R::KeyRole => C::HybridKeyRole,
        R::Expired => C::HybridExpired,
        R::Scope => C::HybridScope,
        R::PreparedFields => C::HybridPreparedFields,
        R::PreparationRefused(_) => C::HybridPreparationRefused,
        R::RefDisclosure => C::HybridRefDisclosure,
        R::RefPinning => C::HybridRefPinning,
        R::SourceSelection => C::HybridSourceSelection,
        R::ImportSourceRequiresCommit => C::HybridImportSourceRequiresCommit,
        R::OperationIdReused => C::HybridOperationIdReused,
        R::PendingOperation => C::HybridPendingOperation,

        R::ValidityBounds => C::HybridValidityBounds,
        R::GenesisBinding => C::HybridGenesisBinding,
        R::ImportPermission => C::HybridImportPermission,
        R::StaleManifest => C::HybridStaleManifest,
        R::CommittedSlot => C::HybridCommittedSlot,
        R::StaleContext => C::HybridStaleContext,
        R::Proof => C::HybridProof,
        R::Revoked => C::HybridRevoked,
        R::SlotConflict => C::HybridSlotConflict,
        R::BoundaryAcceptance => C::HybridBoundaryAcceptance,
        R::Protocol => C::HybridProtocol,
    }
}

/// Exact API preparation refusal, kept distinct from its failure category.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreparationRefusalReason {
    /// API Unspecified refusal.
    Unspecified,
    /// API InvalidScope refusal.
    InvalidScope,
    /// API UnsupportedConverter refusal.
    UnsupportedConverter,
    /// API UnsupportedOptions refusal.
    UnsupportedOptions,
    /// API BudgetExceeded refusal.
    BudgetExceeded,
    /// API DestinationConflict refusal.
    DestinationConflict,
    /// API PolicyDenied refusal.
    PolicyDenied,
}
impl From<crate::wire::ImportPreparationRefusalReason> for PreparationRefusalReason {
    fn from(reason: crate::wire::ImportPreparationRefusalReason) -> Self {
        use crate::wire::ImportPreparationRefusalReason as R;
        match reason {
            R::Unspecified => Self::Unspecified,
            R::InvalidScope => Self::InvalidScope,
            R::UnsupportedConverter => Self::UnsupportedConverter,
            R::UnsupportedOptions => Self::UnsupportedOptions,
            R::BudgetExceeded => Self::BudgetExceeded,
            R::DestinationConflict => Self::DestinationConflict,
            R::PolicyDenied => Self::PolicyDenied,
        }
    }
}
#[cfg(test)]
mod hybrid_error_tests {
    use heddle_api::hybrid_codec::Reject as R;

    use super::*;
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn new_hybrid_failures_preserve_their_category_and_typed_refusal_detail() {
        let cases = [
            (R::RefDisclosure, "hybrid_ref_disclosure"),
            (R::RefPinning, "hybrid_ref_pinning"),
            (R::SourceSelection, "hybrid_source_selection"),
            (
                R::ImportSourceRequiresCommit,
                "hybrid_import_source_requires_commit",
            ),
            (R::OperationIdReused, "hybrid_operation_id_reused"),
            (R::PendingOperation, "hybrid_pending_operation"),
        ];
        for (reject, code) in cases {
            let value = serde_json::to_value(VerificationError::from(Error::Hybrid(reject)))
                .expect("structured error");
            assert_eq!(value["code"], code);
            assert!(value.get("preparation_refusal_reason").is_none());
        }
        for reason in [
            crate::wire::ImportPreparationRefusalReason::Unspecified,
            crate::wire::ImportPreparationRefusalReason::InvalidScope,
            crate::wire::ImportPreparationRefusalReason::UnsupportedConverter,
            crate::wire::ImportPreparationRefusalReason::UnsupportedOptions,
            crate::wire::ImportPreparationRefusalReason::BudgetExceeded,
            crate::wire::ImportPreparationRefusalReason::DestinationConflict,
            crate::wire::ImportPreparationRefusalReason::PolicyDenied,
        ] {
            let value = serde_json::to_value(VerificationError::from(Error::Hybrid(
                R::PreparationRefused(reason),
            )))
            .expect("typed refusal");
            assert_eq!(value["code"], "hybrid_preparation_refused");
            assert_eq!(
                value["preparation_refusal_reason"],
                serde_json::to_value(PreparationRefusalReason::from(reason)).expect("reason")
            );
        }
    }
}
