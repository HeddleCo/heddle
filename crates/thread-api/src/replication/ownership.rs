//! Portable claims retain both original signatures and optionally matched first
//! admission testimony. Structural verification never enrolls an issuer.
use std::collections::BTreeMap;

use crypto::{
    thread_authority_admission::SignedAuthorityAdmission,
    thread_ownership_claim::SignedOwnershipClaim,
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
