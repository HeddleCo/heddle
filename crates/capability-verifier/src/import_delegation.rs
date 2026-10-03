//! Portable owner → device permission → import job authority.
//!
//! Part 2 supplies an independently selected clone keyring and accepted owner
//! state at each mutation. Current checks use receiver time. Historical checks
//! take time only from an API-authenticated exact witness statement. Neither
//! entry point enrolls an incoming account, root, device, or witness.
use std::collections::BTreeSet;

use heddle_api::{
    heddle::api::common::SignedHostedWitnessStatementV1,
    import_authority::{self as contract, ImportOwnerExpectation},
    witness_trust::{self, ResolvedWitnessStatement, VerifiedWitnessSet},
};

use crate::{
    Error, Result, VerificationLimits, VerifiedCloneKeyring, VerifiedOwnerState,
    wire::{
        ImportIdentityV1, ImportOwnerChainV1, SignedDelegatedImportOperationV1,
        SignedImportJobDelegationV1, SignedImportMemberPermissionV1,
    },
};

#[cfg(test)]
#[path = "import_delegation_tests.rs"]
mod tests;

/// Receiver-selected immutable lineage and accepted authority. These are local
/// trust inputs, never values copied from the certificate being verified.
pub struct Selection<'a> {
    /// Selected immutable Spool genesis digest in the owner-record namespace.
    pub spool_genesis_digest: &'a [u8; 32],
    /// Selected original owner id, independently pinned before receiving proofs.
    pub initial_owner_id: &'a [u8; 32],
    /// Selected accepted owner state, including any verified ownership handoffs.
    pub owner: &'a VerifiedOwnerState,
    /// Verified immutable lineage and complete ownership handoffs.
    pub keyring: &'a VerifiedCloneKeyring,
    /// Bounded owner-history verification settings.
    pub limits: VerificationLimits,
}

/// Revocation lookups stay in their own namespaces; cancellation bytes must
/// never be interpreted as owner key ids or ordinary credential revocations.
#[derive(Clone, Copy)]
pub enum Revocation<'a> {
    /// Exact api-defined import cancellation namespace and signed identifier.
    Cancellation(&'static str, &'a [u8]),
    /// Owner/device/job authorization key id in the owner key namespace.
    Key(&'a [u8]),
}

/// Current policy/order is selected by the receiver, separately from the signed
/// import certificate. Current RPC credentials, leases and frontier CAS remain
/// caller gates, as they are not portable import delegation powers.
pub struct CurrentContext<'a> {
    /// Selected public owner lineage.
    pub selection: Selection<'a>,
    /// Actual receiver clock in seconds; never a claimed author time.
    pub now: i64,
    /// Descriptor root and every witness key, including retired/revoked keys.
    pub forbidden_job_keys: &'a [Vec<u8>],
    /// Durable job-key → logical-job associations, retained across expiry.
    pub known_job_associations: &'a [(Vec<u8>, Vec<u8>)],
}

/// Signature/scope permission, distinct from publication and current permission.
/// Retains the complete original signed certificate without re-signing it.
#[derive(Clone)]
pub struct VerifiedImportDelegation {
    signed: SignedImportJobDelegationV1,
    verified: contract::VerifiedImportDelegation,
}

impl VerifiedImportDelegation {
    /// Exact original certificate and signature for export and renewal.
    pub fn signed(&self) -> &SignedImportJobDelegationV1 {
        &self.signed
    }
    /// API-owned typed scope, usable by crypto and the native content model.
    pub fn scope(&self) -> &contract::VerifiedImportDelegation {
        &self.verified
    }
}

fn expectation(selection: &Selection<'_>, now: i64) -> Result<(ImportIdentityV1, Vec<u8>, i64)> {
    let keyring = selection.keyring;
    let genesis = keyring
        .owner_genesis()
        .signed()
        .genesis
        .as_ref()
        .ok_or_else(|| Error::BrokenChain("verified Spool genesis missing".into()))?;
    let digest = crate::creation::spool_genesis_digest(genesis)?;
    if &digest != selection.spool_genesis_digest
        || &keyring.owner_state().owner_id() != selection.initial_owner_id
    {
        return Err(Error::Hybrid(contract::Reject::Root));
    }
    keyring.verify_current_owner(selection.owner, now, selection.limits)?;
    let owner = selection.owner;
    let root = owner
        .signed_root()
        .root
        .as_ref()
        .ok_or_else(|| Error::BrokenChain("verified owner root missing".into()))?;
    // This requires the active owner issuer. Historical callers supply the
    // exact accepted state, rather than reviving an old issuer in today's state.
    owner.issuer_at(&owner.state_hash(), now)?;
    let mut hashes = BTreeSet::from([
        keyring.owner_state().state_hash().to_vec(),
        owner.state_hash().to_vec(),
    ]);
    hashes.extend(
        keyring
            .wire()
            .transfer_owner_histories
            .iter()
            .map(|h| h.state_hash.clone()),
    );
    let chain = ImportOwnerChainV1 {
        spool_genesis_digest: digest.to_vec(),
        owner_state_hashes: hashes.into_iter().collect(),
        transfer_audit_hashes: keyring
            .wire()
            .ownership_transfers
            .iter()
            .map(|a| a.audit_record_hash.clone())
            .collect(),
    };
    let identity = ImportIdentityV1 {
        spool_uuid: keyring.owner_genesis().spool_uuid().to_vec(),
        spool_genesis_digest: digest.to_vec(),
        owner_id: owner.owner_id().to_vec(),
        owner_account_uuid: root.account_uuid.clone(),
        owner_state_hash: owner.state_hash().to_vec(),
        ownership_transfer_sequence: keyring.wire().ownership_transfers.len() as u64,
    };
    let expiry = if root.claimable_deferred_human && root.claimable_until_unix_seconds > 0 {
        root.claimable_until_unix_seconds
    } else {
        i64::MAX
    };
    Ok((identity, contract::owner_chain_digest(&chain)?, expiry))
}

fn verify_at(
    signed: &SignedImportJobDelegationV1,
    member: Option<&SignedImportMemberPermissionV1>,
    context: &CurrentContext<'_>,
    now: i64,
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedImportDelegation> {
    let (identity, digest, expiry) = expectation(&context.selection, now)?;
    let mut forbidden = context.forbidden_job_keys.to_vec();
    forbidden.extend(context.selection.owner.authority_public_keys());
    forbidden.extend(
        context
            .selection
            .keyring
            .owner_state()
            .authority_public_keys(),
    );
    let expected = ImportOwnerExpectation {
        identity: &identity,
        owner_public_key: &context.selection.owner.authority_key().public_key,
        owner_chain_digest: &digest,
        authority_expires_at_seconds: expiry,
        now_unix_seconds: now,
        forbidden_job_keys: &forbidden,
        known_job_associations: context.known_job_associations,
    };
    let verified = contract::verify_delegation(signed, member, &expected)?;
    let body = verified.body();
    let owner_id = heddle_api::hybrid_codec::key_id(expected.owner_public_key);
    let device_id = heddle_api::hybrid_codec::key_id(&body.delegating_public_key);
    if is_revoked(Revocation::Key(&owner_id))
        || is_revoked(Revocation::Key(&device_id))
        || is_revoked(Revocation::Key(&body.job_key_id))
        || is_revoked(Revocation::Cancellation(
            contract::CANCELLATION_NAMESPACE,
            &body.cancellation_id,
        ))
    {
        return Err(Error::Hybrid(contract::Reject::Revoked));
    }
    if let Some(parent) = member {
        let body = parent
            .body
            .as_ref()
            .ok_or(Error::Hybrid(contract::Reject::ImportPermission))?;
        if is_revoked(Revocation::Cancellation(
            contract::CANCELLATION_NAMESPACE,
            &body.cancellation_id,
        )) {
            return Err(Error::Hybrid(contract::Reject::Revoked));
        }
    }
    Ok(VerifiedImportDelegation {
        signed: signed.clone(),
        verified,
    })
}

/// Authorize a certificate for new work using current owner/time/revocations.
pub fn verify_current(
    signed: &SignedImportJobDelegationV1,
    member: Option<&SignedImportMemberPermissionV1>,
    context: &CurrentContext<'_>,
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedImportDelegation> {
    verify_at(signed, member, context, context.now, is_revoked)
}

/// Check an exact retained certificate at an authenticated observation. The
/// callback resolves revocations at that accepted order, not today's policy.
/// The API resolver must authenticate the witness before this entry point;
/// its opaque context is rechecked here, and author time is never consulted.
#[allow(clippy::too_many_arguments)]
pub fn verify_historical(
    signed: &SignedImportJobDelegationV1,
    member: Option<&SignedImportMemberPermissionV1>,
    context: &CurrentContext<'_>,
    statement: &SignedHostedWitnessStatementV1,
    resolved: &ResolvedWitnessStatement,
    set: &VerifiedWitnessSet,
    is_revoked_at_accepted_order: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedImportDelegation> {
    let now_ms = context
        .now
        .checked_mul(1000)
        .ok_or(Error::Hybrid(contract::Reject::Bounds))?;
    witness_trust::recheck_context(resolved, set, statement, now_ms)?;
    let statement = statement
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    if statement.basis != 1 {
        return Err(Error::BoundaryAcceptancePendingApi318);
    }
    if statement.purpose != 3
        || statement.authority_digest != contract::signed_delegation_digest(signed)?
    {
        return Err(Error::Hybrid(contract::Reject::Scope));
    }
    let (identity, _, _) =
        expectation(&context.selection, statement.observed_at_unix_millis / 1000)?;
    if statement.spool_uuid != identity.spool_uuid
        || statement.spool_genesis_digest != identity.spool_genesis_digest
        || statement.owner_id != identity.owner_id
        || statement.owner_state_hash != identity.owner_state_hash
        || statement.ownership_transfer_sequence != identity.ownership_transfer_sequence
    {
        return Err(Error::Hybrid(contract::Reject::Root));
    }
    verify_at(
        signed,
        member,
        context,
        statement.observed_at_unix_millis / 1000,
        is_revoked_at_accepted_order,
    )
}

/// Verify the original genesis certificate at its own exact authenticated first
/// admission. Renewal certificates do not rewrite the original binding.
#[allow(clippy::too_many_arguments)]
pub fn verify_historical_genesis(
    signed: &SignedImportJobDelegationV1,
    member: Option<&SignedImportMemberPermissionV1>,
    context: &CurrentContext<'_>,
    payload: &crate::wire::ImportGenesisWitnessV1,
    statement: &SignedHostedWitnessStatementV1,
    resolved: &ResolvedWitnessStatement,
    set: &VerifiedWitnessSet,
    is_revoked_at_accepted_order: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedImportDelegation> {
    witness_trust::recheck_context(
        resolved,
        set,
        statement,
        context
            .now
            .checked_mul(1000)
            .ok_or(Error::Hybrid(contract::Reject::Bounds))?,
    )?;
    let s = statement
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    if s.basis != 1 {
        return Err(Error::BoundaryAcceptancePendingApi318);
    }
    contract::verify_witness_payload(s, contract::WitnessPayload::Genesis(payload))?;
    let verified = verify_at(
        signed,
        member,
        context,
        s.observed_at_unix_millis / 1000,
        is_revoked_at_accepted_order,
    )?;
    let binding = payload
        .binding
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    let body = binding
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    contract::verify_genesis_authority(
        binding,
        &verified.verified,
        &body.genesis_digest,
        &body.original_creator_signature,
        &heddle_api::hybrid_codec::hash(&[&payload.creator_authority_envelope]),
    )?;
    Ok(verified)
}

/// Check a new job result, rechecking the complete current certificate first.
pub fn verify_new_operation(
    operation: &SignedDelegatedImportOperationV1,
    delegation: &VerifiedImportDelegation,
    member: Option<&SignedImportMemberPermissionV1>,
    context: &CurrentContext<'_>,
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<()> {
    let current = verify_current(&delegation.signed, member, context, is_revoked)?;
    contract::verify_new_operation(operation, &current.verified, context.now)?;
    Ok(())
}
