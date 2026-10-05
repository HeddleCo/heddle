//! Exact ownership proof framing. Verification never enrolls its carried keys.
use crypto::{
    thread_ownership_claim::{SignedOwnershipAcceptance, SignedOwnershipClaim},
    thread_ownership_resolution::SignedOwnershipResolution,
};
use heddle_object_model::object::thread_replication::{
    ownership_claim::{FORMAT, ThreadOwnershipClaim},
    ownership_resolution::{FORMAT as RESOLUTION_FORMAT, ThreadOwnershipResolution},
};

use crate::{
    contract::{RecordSignature, SignedRecord},
    transport::Error,
};

pub enum ClaimProof {
    Acceptance(SignedOwnershipAcceptance),
    Complete(SignedOwnershipClaim),
}
pub fn decode(record: &SignedRecord) -> Result<ClaimProof, Error> {
    if record.format != FORMAT {
        return Err(Error::Protocol("ownership claim format required"));
    }
    let value = ThreadOwnershipClaim::decode(&record.canonical_record)
        .map_err(|_| Error::Protocol("invalid canonical ownership claim"))?;
    match record.signatures.as_slice() {
        [acceptor] if acceptor.public_key == value.accepting_publisher => {
            let proof = SignedOwnershipAcceptance {
                canonical: record.canonical_record.clone(),
                signature: acceptor.signature.clone(),
            };
            proof
                .verify()
                .map_err(|_| Error::Protocol("invalid account acceptance signature"))?;
            Ok(ClaimProof::Acceptance(proof))
        }
        [_, _] if record.signatures[0].public_key < record.signatures[1].public_key => {
            let local = record
                .signatures
                .iter()
                .find(|s| s.public_key == value.prior_local_key)
                .ok_or(Error::Protocol("local ownership signature missing"))?;
            let acceptor = record
                .signatures
                .iter()
                .find(|s| s.public_key == value.accepting_publisher)
                .ok_or(Error::Protocol("accepting ownership signature missing"))?;
            let proof = SignedOwnershipClaim {
                canonical: record.canonical_record.clone(),
                local_signature: local.signature.clone(),
                acceptance_signature: acceptor.signature.clone(),
            };
            proof
                .verify()
                .map_err(|_| Error::Protocol("invalid dual ownership signatures"))?;
            Ok(ClaimProof::Complete(proof))
        }
        _ => Err(Error::Protocol(
            "ownership requires acceptor or ordered owner and acceptor signatures",
        )),
    }
}
pub fn encode(proof: &SignedOwnershipClaim) -> Result<SignedRecord, Error> {
    let value = proof
        .verify()
        .map_err(|_| Error::Protocol("invalid dual ownership signatures"))?;
    let mut signatures = vec![
        RecordSignature {
            public_key: value.prior_local_key.to_vec(),
            signature: proof.local_signature.clone(),
        },
        RecordSignature {
            public_key: value.accepting_publisher.to_vec(),
            signature: proof.acceptance_signature.clone(),
        },
    ];
    signatures.sort_by(|a, b| a.public_key.cmp(&b.public_key));
    Ok(SignedRecord {
        format: FORMAT.into(),
        canonical_record: proof.canonical.clone(),
        signatures,
    })
}
pub fn encode_acceptance(proof: &SignedOwnershipAcceptance) -> Result<SignedRecord, Error> {
    let value = proof
        .verify()
        .map_err(|_| Error::Protocol("invalid account acceptance signature"))?;
    Ok(SignedRecord {
        format: FORMAT.into(),
        canonical_record: proof.canonical.clone(),
        signatures: vec![RecordSignature {
            public_key: value.accepting_publisher.to_vec(),
            signature: proof.signature.clone(),
        }],
    })
}
pub fn decode_resolution(record: &SignedRecord) -> Result<SignedOwnershipResolution, Error> {
    if record.format != RESOLUTION_FORMAT {
        return Err(Error::Protocol("ownership resolution format required"));
    }
    let value = ThreadOwnershipResolution::decode(&record.canonical_record)
        .map_err(|_| Error::Protocol("invalid canonical ownership resolution"))?;
    if record.signatures.len() != 2
        || record.signatures[0].public_key >= record.signatures[1].public_key
    {
        return Err(Error::Protocol(
            "resolution requires two canonical ordered signatures",
        ));
    }
    let local = record
        .signatures
        .iter()
        .find(|s| s.public_key == value.local_owner)
        .ok_or(Error::Protocol("local resolution signature missing"))?;
    let acceptor = record
        .signatures
        .iter()
        .find(|s| s.public_key == value.accepting_publisher)
        .ok_or(Error::Protocol("accepting resolution signature missing"))?;
    Ok(SignedOwnershipResolution {
        canonical: record.canonical_record.clone(),
        local_signature: local.signature.clone(),
        acceptance_signature: acceptor.signature.clone(),
    })
}
pub fn encode_resolution(proof: &SignedOwnershipResolution) -> Result<SignedRecord, Error> {
    let value = ThreadOwnershipResolution::decode(&proof.canonical)
        .map_err(|_| Error::Protocol("invalid ownership resolution"))?;
    let mut record = SignedRecord {
        format: RESOLUTION_FORMAT.into(),
        canonical_record: proof.canonical.clone(),
        signatures: vec![
            RecordSignature {
                public_key: value.local_owner.to_vec(),
                signature: proof.local_signature.clone(),
            },
            RecordSignature {
                public_key: value.accepting_publisher.to_vec(),
                signature: proof.acceptance_signature.clone(),
            },
        ],
    };
    record
        .signatures
        .sort_by(|a, b| a.public_key.cmp(&b.public_key));
    Ok(record)
}
