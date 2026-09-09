//! Exact ownership proof framing. Verification never enrolls its carried keys.
use crypto::thread_ownership_claim::{SignedOwnershipAcceptance, SignedOwnershipClaim};
use heddle_object_model::object::thread_replication::ownership_claim::{FORMAT, ThreadOwnershipClaim};
use crate::{contract::{RecordSignature, SignedRecord}, transport::Error};

pub enum ClaimProof { Acceptance(SignedOwnershipAcceptance), Complete(SignedOwnershipClaim) }
pub fn decode(record: &SignedRecord) -> Result<ClaimProof, Error> {
    if record.format != FORMAT { return Err(Error::Protocol("ownership claim format required")); }
    let value = ThreadOwnershipClaim::decode(&record.canonical_record).map_err(|_| Error::Protocol("invalid canonical ownership claim"))?;
    match record.signatures.as_slice() {
        [acceptor] if acceptor.public_key == value.accepting_publisher => {
            let proof = SignedOwnershipAcceptance { canonical: record.canonical_record.clone(), signature: acceptor.signature.clone() };
            proof.verify().map_err(|_| Error::Protocol("invalid account acceptance signature"))?;
            Ok(ClaimProof::Acceptance(proof))
        }
        [local, acceptor] if local.public_key == value.prior_local_key && acceptor.public_key == value.accepting_publisher => {
            let proof = SignedOwnershipClaim { canonical: record.canonical_record.clone(), local_signature: local.signature.clone(), acceptance_signature: acceptor.signature.clone() };
            proof.verify().map_err(|_| Error::Protocol("invalid dual ownership signatures"))?;
            Ok(ClaimProof::Complete(proof))
        }
        _ => Err(Error::Protocol("ownership requires acceptor or ordered owner and acceptor signatures")),
    }
}
pub fn encode(proof: &SignedOwnershipClaim) -> Result<SignedRecord, Error> {
    let value = proof.verify().map_err(|_| Error::Protocol("invalid dual ownership signatures"))?;
    Ok(SignedRecord { format: FORMAT.into(), canonical_record: proof.canonical.clone(), signatures: vec![
        RecordSignature { public_key: value.prior_local_key.to_vec(), signature: proof.local_signature.clone() },
        RecordSignature { public_key: value.accepting_publisher.to_vec(), signature: proof.acceptance_signature.clone() },
    ] })
}
pub fn encode_acceptance(proof: &SignedOwnershipAcceptance) -> Result<SignedRecord, Error> {
    let value = proof.verify().map_err(|_| Error::Protocol("invalid account acceptance signature"))?;
    Ok(SignedRecord { format: FORMAT.into(), canonical_record: proof.canonical.clone(), signatures: vec![
        RecordSignature { public_key: value.accepting_publisher.to_vec(), signature: proof.signature.clone() },
    ] })
}
