//! Portable claims retain both original signatures and optionally matched first
//! admission testimony. Structural verification never enrolls an issuer.
use std::collections::BTreeMap;

use crypto::{
    thread_authority_admission::SignedAuthorityAdmission,
    thread_ownership_claim::SignedOwnershipClaim,
    thread_ownership_resolution::SignedOwnershipResolution,
};
use heddle_object_model::object::{
    ContentHash,
    thread_replication::{ThreadGenesis, integration::TrustedHostedExecutor},
};

use crate::{contract::ThreadGenesisRecord, transport::Error};

pub struct OriginalClaim {
    pub original: SignedOwnershipClaim,
    pub authority_admission: Option<SignedAuthorityAdmission>,
}
pub struct OriginalResolution {
    pub original: SignedOwnershipResolution,
    pub authority_admission: Option<SignedAuthorityAdmission>,
}
pub fn verify_resolutions(
    record: &ThreadGenesisRecord,
    genesis: &ThreadGenesis,
) -> Result<Vec<OriginalResolution>, Error> {
    if record.ownership_resolutions.len() > 1 || record.ownership_resolution_admissions.len() > 1 {
        return Err(Error::Protocol(
            "ownership resolution witness exceeds bound",
        ));
    }
    let claims = verify_claims(record, genesis)?;
    let mut evidence = crate::boundary_acceptance::wrapper_evidence(record)?;
    let admission = record
        .ownership_resolution_admissions
        .first()
        .map(|wire| {
            let mut receipt = crate::authority_admission::decode(wire)?;
            let statement = receipt
                .verify_signature()
                .map_err(|_| Error::Protocol("invalid resolution admission signature"))?;
            receipt.boundary_acceptance = evidence.matched(&statement.basis)?;
            Ok::<_, Error>((receipt, statement))
        })
        .transpose()?;
    let Some(wire) = record.ownership_resolutions.first() else {
        if admission.is_some() {
            return Err(Error::Protocol("unmatched resolution admission"));
        }
        return Ok(Vec::new());
    };
    let original = crate::thread_ownership::decode_resolution(wire)?;
    let value = heddle_object_model::object::thread_replication::ownership_resolution::ThreadOwnershipResolution::decode(&original.canonical)
        .map_err(|_| Error::Protocol("invalid ownership resolution"))?;
    value
        .validate_genesis(genesis)
        .map_err(|_| Error::Protocol("resolution differs from immutable genesis"))?;
    let claim_ids = claims
        .iter()
        .map(|claim| {
            claim
                .original
                .verify()
                .and_then(|value| value.id().map_err(Into::into))
        })
        .collect::<Result<std::collections::BTreeSet<_>, _>>()
        .map_err(|_| Error::Protocol("invalid ownership claims"))?;
    if claim_ids != value.conflicting_claims {
        return Err(Error::Protocol(
            "resolution differs from exact bundled claim set",
        ));
    }
    let winner = claims
        .iter()
        .find(|claim| {
            claim
                .original
                .verify()
                .and_then(|value| value.id().map_err(Into::into))
                .ok()
                == Some(value.winning_claim)
        })
        .ok_or(Error::Protocol("winning claim absent"))?;
    original
        .verify(
            &winner
                .original
                .verify()
                .map_err(|_| Error::Protocol("invalid winning claim"))?,
        )
        .map_err(|_| Error::Protocol("invalid dual resolution signatures"))?;
    let authority_admission = admission
        .map(|(receipt, statement)| {
            let trust = TrustedHostedExecutor {
                spool: statement.spool,
                spool_genesis: statement.spool_genesis,
                executor: statement.executor,
            };
            receipt
                .verify_resolution(
                    &original,
                    &winner
                        .original
                        .verify()
                        .map_err(|_| Error::Protocol("invalid winning claim"))?,
                    genesis,
                    &trust,
                )
                .map_err(|_| Error::Protocol("resolution admission differs from original"))?;
            Ok::<_, Error>(receipt)
        })
        .transpose()?;
    evidence.finish()?;
    Ok(vec![OriginalResolution {
        original,
        authority_admission,
    }])
}
/// Conflicts are preserved, never selected by arrival order. The caller admits
/// each claim independently and treats multiple accepted claims as unavailable.
pub fn verify_claims(
    record: &ThreadGenesisRecord,
    genesis: &ThreadGenesis,
) -> Result<Vec<OriginalClaim>, Error> {
    if record.ownership_claims.len() > 2 || record.ownership_claim_admissions.len() > 2 {
        return Err(Error::Protocol("ownership claim witness exceeds bound"));
    }
    let mut evidence = crate::boundary_acceptance::wrapper_evidence(record)?;
    let mut receipts = BTreeMap::new();
    for wire in &record.ownership_claim_admissions {
        let mut receipt = crate::authority_admission::decode(wire)?;
        let statement = receipt
            .verify_signature()
            .map_err(|_| Error::Protocol("invalid claim admission signature"))?;
        receipt.boundary_acceptance = evidence.matched(&statement.basis)?;
        let id = statement.subject.claim_id().ok_or(Error::Protocol(
            "claim witness cannot carry operation admission",
        ))?;
        if receipts.insert(id, (receipt, statement)).is_some() {
            return Err(Error::Protocol("duplicate claim admission"));
        }
    }
    let mut ids = std::collections::BTreeSet::<ContentHash>::new();
    let mut output = Vec::new();
    for wire in &record.ownership_claims {
        let crate::thread_ownership::ClaimProof::Complete(original) =
            crate::thread_ownership::decode(wire)?
        else {
            return Err(Error::Protocol(
                "transferred claim requires both original signatures",
            ));
        };
        let claim = original
            .verify()
            .map_err(|_| Error::Protocol("invalid original claim"))?;
        claim
            .validate_genesis(genesis)
            .map_err(|_| Error::Protocol("claim differs from immutable genesis"))?;
        let id = claim
            .id()
            .map_err(|_| Error::Protocol("invalid claim identity"))?;
        if !ids.insert(id) {
            return Err(Error::Protocol("duplicate original ownership claim"));
        }
        let admission = receipts
            .remove(&id)
            .map(
                |(receipt, statement)| -> Result<SignedAuthorityAdmission, Error> {
                    let trust = TrustedHostedExecutor {
                        spool: statement.spool,
                        spool_genesis: statement.spool_genesis,
                        executor: statement.executor,
                    };
                    // Only compare the immutable subject here. Independent receiver pins
                    // remain mandatory in the storage admission path.
                    receipt
                        .verify_claim(&original, genesis, &trust)
                        .map_err(|_| Error::Protocol("claim admission differs from original"))?;
                    Ok(receipt)
                },
            )
            .transpose()?;
        output.push(OriginalClaim {
            original,
            authority_admission: admission,
        });
    }
    if !receipts.is_empty() {
        return Err(Error::Protocol("unmatched ownership claim admission"));
    }
    evidence.finish()?;
    Ok(output)
}
