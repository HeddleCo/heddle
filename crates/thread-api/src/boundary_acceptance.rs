//! Structural acceptance evidence matching, never executor trust or fresh authority.
use std::collections::{BTreeMap, BTreeSet};

use crypto::original_boundary_acceptance::SignedBoundaryAcceptance;
use heddle_object_model::object::{
    ContentHash,
    original_boundary_acceptance::{AdmissionBasis, FORMAT, MAX_ACCEPTANCE_BYTES},
};

use crate::{contract as wire, transport::Error};

pub const MAX_ACCEPTANCES: usize = 128;
pub fn decode(record: &wire::SignedRecord) -> Result<SignedBoundaryAcceptance, Error> {
    if record.format != FORMAT
        || record.canonical_record.is_empty()
        || record.canonical_record.len() > MAX_ACCEPTANCE_BYTES
    {
        return Err(Error::Protocol(
            "invalid boundary acceptance format or bound",
        ));
    }
    let [signature] = record.signatures.as_slice() else {
        return Err(Error::Protocol(
            "one boundary acceptance signature required",
        ));
    };
    if signature.public_key.len() != 32 || signature.signature.len() != 64 {
        return Err(Error::Protocol(
            "invalid boundary acceptance signature shape",
        ));
    }
    let signed = SignedBoundaryAcceptance {
        canonical: record.canonical_record.clone(),
        signature: signature.signature.clone(),
    };
    let value = signed
        .verify_signature()
        .map_err(|_| Error::Protocol("invalid boundary acceptance signature"))?;
    if signature.public_key != value.accepting_publisher {
        return Err(Error::Protocol("boundary acceptance signer differs"));
    }
    Ok(signed)
}
pub fn encode(signed: &SignedBoundaryAcceptance) -> Result<wire::SignedRecord, Error> {
    let value = signed
        .verify_signature()
        .map_err(|_| Error::Protocol("invalid boundary acceptance signature"))?;
    Ok(wire::SignedRecord {
        format: FORMAT.into(),
        canonical_record: signed.canonical.clone(),
        signatures: vec![wire::RecordSignature {
            public_key: value.accepting_publisher.to_vec(),
            signature: signed.signature.clone(),
        }],
    })
}
#[derive(Default)]
pub struct Evidence {
    values: BTreeMap<ContentHash, std::sync::Arc<SignedBoundaryAcceptance>>,
    used: BTreeSet<ContentHash>,
}
impl Evidence {
    pub fn add(&mut self, records: &[wire::SignedRecord]) -> Result<(), Error> {
        if records.len() > MAX_ACCEPTANCES {
            return Err(Error::Protocol("boundary evidence count exceeded"));
        }
        let mut local = BTreeSet::new();
        for record in records {
            let signed = decode(record)?;
            let id = ContentHash::compute_typed(FORMAT, &signed.canonical);
            if !local.insert(id) {
                return Err(Error::Protocol("duplicate boundary evidence in carrier"));
            }
            if let Some(prior) = self.values.get(&id) {
                if prior.as_ref() != &signed {
                    return Err(Error::Protocol("conflicting boundary evidence"));
                }
            } else {
                if self.values.len() >= MAX_ACCEPTANCES {
                    return Err(Error::Protocol("boundary evidence count exceeded"));
                }
                self.values.insert(id, std::sync::Arc::new(signed));
            }
        }
        Ok(())
    }
    pub fn matched(
        &mut self,
        basis: &AdmissionBasis,
    ) -> Result<Option<std::sync::Arc<SignedBoundaryAcceptance>>, Error> {
        match basis {
            AdmissionBasis::OriginalAuthority => Ok(None),
            AdmissionBasis::BoundaryAcceptance { acceptance } => {
                let value = self
                    .values
                    .get(acceptance)
                    .ok_or(Error::Protocol("missing matched boundary acceptance"))?
                    .clone();
                self.used.insert(*acceptance);
                Ok(Some(value))
            }
        }
    }
    pub fn finish(self) -> Result<(), Error> {
        if self.values.len() != self.used.len() {
            return Err(Error::Protocol("unreferenced boundary acceptance evidence"));
        }
        Ok(())
    }
}
/// All evidence references in a genesis/claim carrier share one bounded matcher.
pub fn wrapper_evidence(record: &wire::ThreadGenesisRecord) -> Result<Evidence, Error> {
    use prost::Message;
    if record.ownership_claims.len() > 2
        || record.ownership_claim_admissions.len() > 2
        || record.encoded_len() > 256 * 1024
    {
        return Err(Error::Protocol("genesis evidence wrapper exceeds bounds"));
    }
    let mut evidence = Evidence::default();
    evidence.add(&record.boundary_acceptances)?;
    if let Some(receipt) = &record.admission {
        let value =
            heddle_object_model::object::thread_genesis_admission::ThreadGenesisAdmission::decode(
                &receipt.canonical_record,
            )
            .map_err(|_| Error::Protocol("invalid genesis admission"))?;
        evidence.matched(&value.basis)?;
    }
    for receipt in &record.ownership_claim_admissions {
        evidence.matched(&crate::authority_admission::verify_signature(receipt)?.basis)?;
    }
    Ok(evidence)
}
/// Wire sidecar for an already matched receipt; full original/pin verification is
/// still required at the receiver before persistence.
pub fn authority_evidence(
    receipt: Option<&crate::authority_admission::SignedAuthorityAdmission>,
) -> Result<Vec<wire::SignedRecord>, Error> {
    receipt
        .and_then(|receipt| receipt.boundary_acceptance.as_deref())
        .map(encode)
        .transpose()
        .map(|value| value.into_iter().collect())
}
/// Decode a genesis receipt and match its evidence, without deriving issuer trust.
pub fn genesis_admission(
    wrapper: &wire::ThreadGenesisRecord,
) -> Result<Option<crypto::thread_genesis_admission::SignedGenesisAdmission>, Error> {
    let mut evidence = wrapper_evidence(wrapper)?;
    let Some(record) = &wrapper.admission else {
        evidence.finish()?;
        return Ok(None);
    };
    if record.format != heddle_object_model::object::thread_genesis_admission::FORMAT {
        return Err(Error::Protocol("invalid genesis receipt format"));
    }
    let [signature] = record.signatures.as_slice() else {
        return Err(Error::Protocol("one genesis receipt signature required"));
    };
    let mut signed = crypto::thread_genesis_admission::SignedGenesisAdmission {
        canonical: record.canonical_record.clone(),
        signature: signature.signature.clone(),
        boundary_acceptance: None,
    };
    let value = signed
        .verify_signature()
        .map_err(|_| Error::Protocol("invalid genesis receipt signature"))?;
    if signature.public_key != value.executor {
        return Err(Error::Protocol("genesis receipt signer differs"));
    }
    signed.boundary_acceptance = evidence.matched(&value.basis)?;
    evidence.finish()?;
    Ok(Some(signed))
}

#[cfg(test)]
#[path = "boundary_acceptance_tests.rs"]
mod tests;
