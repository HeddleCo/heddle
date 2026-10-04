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
#[derive(Clone, Copy)]
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

/// Signature-integrity verification of an exact retained Spool policy. This
/// does not select the accepted policy head or infer operation/revocation order.
/// Current callers select their own head; historical callers use an exact
/// authenticated witness's policy sequence/hash. Successor admission and replay
/// enforce revocation introduction against authenticated predecessor state.
pub fn verify_policy_record(
    signed: &crate::wire::SignedSpoolPolicyRecord,
    owners: &[&VerifiedOwnerState],
) -> Result<()> {
    let body = signed
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    let owner = owners
        .iter()
        .find(|o| o.owner_id().as_slice() == body.owner_id)
        .ok_or(Error::Hybrid(contract::Reject::Root))?;
    let issuer = owner.provenance_issuer(
        &body.owner_state_hash,
        owner.issuers_sequence(&body.owner_state_hash)?,
    )?;
    body.expected_head
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    crate::policy::verify_signed_spool_policy_record_integrity(signed, issuer)?;
    Ok(())
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
    /// Actual receiver clock in milliseconds; never a claimed author time.
    pub now_millis: i64,
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
    member: Option<SignedImportMemberPermissionV1>,
    owner: VerifiedOwnerState,
    spool_path: String,
}

impl VerifiedImportDelegation {
    /// Exact original typed parent; absent only for direct active owner signing.
    pub fn member_permission(&self) -> Option<&SignedImportMemberPermissionV1> {
        self.member.as_ref()
    }
    /// Verified accepted owner state used for this exact certificate.
    pub fn owner(&self) -> &VerifiedOwnerState {
        &self.owner
    }
    /// Independently selected canonical Spool path for ordinary owner evidence.
    pub fn spool_path(&self) -> &str {
        &self.spool_path
    }
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
    let delegator = &signed
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?
        .delegating_public_key;
    let user_keys: Vec<_> = context.selection.owner.authority_public_keys().collect();
    for key in std::iter::once(delegator)
        .chain(user_keys.iter())
        .chain(context.selection.keyring.authority_public_keys())
    {
        if context.forbidden_job_keys.contains(key)
            || context
                .known_job_associations
                .iter()
                .any(|(job, _)| job == key)
        {
            return Err(Error::Hybrid(contract::Reject::KeyRole));
        }
    }
    let (identity, digest, expiry) = expectation(&context.selection, now)?;
    let mut forbidden = context.forbidden_job_keys.to_vec();
    forbidden.extend(context.selection.owner.authority_public_keys());
    forbidden.extend(context.selection.keyring.authority_public_keys().cloned());
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
        member: member.cloned(),
        owner: context.selection.owner.clone(),
        spool_path: context
            .selection
            .keyring
            .wire()
            .canonical_spool_path_segments
            .join("/"),
    })
}

/// Authorize a certificate for new work using current owner/time/revocations.
pub fn verify_current(
    signed: &SignedImportJobDelegationV1,
    member: Option<&SignedImportMemberPermissionV1>,
    context: &CurrentContext<'_>,
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedImportDelegation> {
    verify_at(
        signed,
        member,
        context,
        context.now_millis / 1000,
        is_revoked,
    )
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
    let now_ms = context.now_millis;
    witness_trust::recheck_context(resolved, set, statement, now_ms)?;
    let statement = statement
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
    if statement.basis != 1 {
        return Err(Error::Hybrid(contract::Reject::BoundaryAcceptance));
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
    witness_trust::recheck_context(resolved, set, statement, context.now_millis)?;
    let s = statement
        .body
        .as_ref()
        .ok_or(Error::Hybrid(contract::Reject::Canonical))?;
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
    contract::verify_new_operation(operation, &current.verified, context.now_millis / 1000)?;
    Ok(())
}

/// Verify current import authority from canonical byte and JSON inputs.
/// Shared by the public WASM binding and native differential consumer.
#[allow(clippy::too_many_arguments)]
pub fn verify_bytes(
    certificate: &[u8],
    permission: &[u8],
    keyring: &[u8],
    accepted_owner_history: &[u8],
    selected_initial_owner_id: &[u8],
    selected_spool_genesis_digest: &[u8],
    forbidden_job_keys_json: &str,
    known_job_associations_json: &str,
    cancelled_ids_json: &str,
    revoked_key_ids_json: &str,
    now_unix_seconds: i64,
    max_capability_ttl_seconds: i64,
) -> crate::Result<Vec<u8>> {
    use crate::canonical::{canonical_message, fixed};
    fn ids(value: &str, width: usize) -> crate::Result<Vec<Vec<u8>>> {
        if value.len() > heddle_api::import_authority::MAX_BUNDLE_BYTES {
            return Err(crate::Error::TooLarge {
                limit: heddle_api::import_authority::MAX_BUNDLE_BYTES,
            });
        }
        let values: Vec<String> =
            serde_json::from_str(value).map_err(|e| crate::Error::Invalid(e.to_string()))?;
        if values.len() > 4096 {
            return Err(crate::Error::TooLarge { limit: 4096 });
        }
        values
            .iter()
            .map(|v| {
                let bytes = hex::decode(v).map_err(|e| crate::Error::Invalid(e.to_string()))?;
                if bytes.len() != width {
                    return Err(crate::Error::Invalid("wrong identifier width".into()));
                }
                Ok(bytes)
            })
            .collect()
    }
    (|| -> crate::Result<Vec<u8>> {
        let limits = VerificationLimits::new(max_capability_ttl_seconds)?;
        let keyring = crate::verify_clone_keyring_bytes(keyring, now_unix_seconds, limits, &[])?;
        let history: crate::wire::OwnerHistory =
            canonical_message(accepted_owner_history, limits.max_bundle_bytes())?;
        let mut owner = crate::verify_owner_root(
            history
                .root
                .as_ref()
                .ok_or_else(|| crate::Error::Invalid("owner root missing".into()))?,
        )?;
        if history.accepted_transitions.len() > VerificationLimits::MAX_TRANSITIONS {
            return Err(crate::Error::TooLarge {
                limit: VerificationLimits::MAX_TRANSITIONS,
            });
        }
        for transition in &history.accepted_transitions {
            owner = crate::apply_accepted_transition(&owner, transition, now_unix_seconds, limits)?;
        }
        if history.state_hash != owner.state_hash() {
            return Err(crate::Error::Hybrid(heddle_api::hybrid_codec::Reject::Root));
        }
        let certificate =
            canonical_message(certificate, heddle_api::import_authority::MAX_RECORD_BYTES)?;
        let permission = if permission.is_empty() {
            None
        } else {
            Some(canonical_message(
                permission,
                heddle_api::import_authority::MAX_RECORD_BYTES,
            )?)
        };
        let initial = fixed(selected_initial_owner_id, "initial owner id")?;
        let digest = fixed(selected_spool_genesis_digest, "Spool genesis digest")?;
        let forbidden = ids(forbidden_job_keys_json, 32)?;
        let cancelled = ids(cancelled_ids_json, 32)?;
        let revoked = ids(revoked_key_ids_json, 32)?;
        if known_job_associations_json.len() > heddle_api::import_authority::MAX_BUNDLE_BYTES {
            return Err(crate::Error::TooLarge {
                limit: heddle_api::import_authority::MAX_BUNDLE_BYTES,
            });
        }
        let pairs: Vec<[String; 2]> = serde_json::from_str(known_job_associations_json)
            .map_err(|e| crate::Error::Invalid(e.to_string()))?;
        if pairs.len() > 4096 {
            return Err(crate::Error::TooLarge { limit: 4096 });
        }
        let associations = pairs
            .iter()
            .map(|p| {
                let key = hex::decode(&p[0]).map_err(|e| crate::Error::Invalid(e.to_string()))?;
                let job = hex::decode(&p[1]).map_err(|e| crate::Error::Invalid(e.to_string()))?;
                fixed::<32>(&key, "job key")?;
                fixed::<16>(&job, "logical job")?;
                Ok((key, job))
            })
            .collect::<crate::Result<Vec<_>>>()?;
        let context = CurrentContext {
            selection: Selection {
                spool_genesis_digest: &digest,
                initial_owner_id: &initial,
                owner: &owner,
                keyring: &keyring,
                limits,
            },
            now_millis: now_unix_seconds
                .checked_mul(1000)
                .ok_or(crate::Error::Hybrid(
                    heddle_api::hybrid_codec::Reject::Bounds,
                ))?,
            forbidden_job_keys: &forbidden,
            known_job_associations: &associations,
        };
        let verified = crate::import_delegation::verify_current(
            &certificate,
            permission.as_ref(),
            &context,
            |r| match r {
                Revocation::Cancellation(_, id) => cancelled.iter().any(|c| c == id),
                Revocation::Key(id) => revoked.iter().any(|k| k == id),
            },
        )?;
        Ok(verified.scope().digest().to_vec())
    })()
}
