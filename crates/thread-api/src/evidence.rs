//! Typed immutable evidence codecs shared by hosted and device callers.
//! Signature verification does not establish current account or Spool authority.
use crypto::{Ed25519Signer, Signer};
use heddle_object_model::object::check_evidence::{ACKNOWLEDGEMENT_FORMAT, EVIDENCE_FORMAT};
pub use heddle_object_model::object::check_evidence::{
    CheckAcknowledgement, CheckAuthor, CheckEvidence, CheckOutcome,
};

use crate::{contract as wire, transport::Error};

pub fn sign_evidence(
    value: &CheckEvidence,
    signer: &impl Signer,
) -> Result<wire::SignedRecord, Error> {
    sign(
        EVIDENCE_FORMAT,
        value
            .encode()
            .map_err(|_| Error::Protocol("invalid check evidence"))?,
        &value.author,
        signer,
    )
}
pub fn verify_evidence(record: &wire::SignedRecord) -> Result<CheckEvidence, Error> {
    let value = CheckEvidence::decode(&record.canonical_record)
        .map_err(|_| Error::Protocol("invalid canonical check evidence"))?;
    verify(record, EVIDENCE_FORMAT, &value.author)?;
    Ok(value)
}
pub fn sign_acknowledgement(
    value: &CheckAcknowledgement,
    signer: &impl Signer,
) -> Result<wire::SignedRecord, Error> {
    sign(
        ACKNOWLEDGEMENT_FORMAT,
        value
            .encode()
            .map_err(|_| Error::Protocol("invalid check acknowledgement"))?,
        &value.author,
        signer,
    )
}
pub fn verify_acknowledgement(record: &wire::SignedRecord) -> Result<CheckAcknowledgement, Error> {
    let value = CheckAcknowledgement::decode(&record.canonical_record)
        .map_err(|_| Error::Protocol("invalid canonical check acknowledgement"))?;
    verify(record, ACKNOWLEDGEMENT_FORMAT, &value.author)?;
    Ok(value)
}
fn sign(
    format: &str,
    canonical: Vec<u8>,
    author: &CheckAuthor,
    signer: &impl Signer,
) -> Result<wire::SignedRecord, Error> {
    if signer.public_key() != author.publisher {
        return Err(Error::Protocol("check publisher differs from signer"));
    }
    let signature = signer
        .sign(&signing_bytes(format, &canonical))
        .map_err(|_| Error::Protocol("check signing failed"))?;
    let record = wire::SignedRecord {
        format: format.into(),
        canonical_record: canonical,
        signatures: vec![wire::RecordSignature {
            public_key: author.publisher.to_vec(),
            signature,
        }],
    };
    verify(&record, format, author)?;
    Ok(record)
}
fn verify(record: &wire::SignedRecord, format: &str, author: &CheckAuthor) -> Result<(), Error> {
    let [signature] = record.signatures.as_slice() else {
        return Err(Error::Protocol(
            "check requires exactly one original signature",
        ));
    };
    if record.format != format || signature.public_key != author.publisher {
        return Err(Error::Protocol("check format or publisher mismatch"));
    }
    Ed25519Signer::verify_with_public_key(
        &signing_bytes(format, &record.canonical_record),
        &author.publisher,
        &signature.signature,
    )
    .map_err(|_| Error::Protocol("invalid check signature"))
}
fn signing_bytes(format: &str, canonical: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(format.len() + 1 + canonical.len());
    bytes.extend_from_slice(format.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(canonical);
    bytes
}

#[cfg(test)]
mod tests {
    use heddle_object_model::object::{
        CollaborationActor, ContentHash, StateId, thread_replication::metadata::AUTHORITY_FORMAT,
    };
    use uuid::Uuid;

    use super::*;

    fn fixture() -> (CheckEvidence, Ed25519Signer) {
        let signer = Ed25519Signer::generate().expect("signer");
        let envelope = b"independently verified by receiving host".to_vec();
        (
            CheckEvidence {
                version: 1,
                id: Uuid::from_u128(1),
                spool: Uuid::from_u128(2),
                revision: StateId::from_bytes([3; 32]),
                check: "unit-tests".into(),
                outcome: CheckOutcome::Passed,
                detail: "42 passed".into(),
                artifacts: Vec::new(),
                author: CheckAuthor {
                    actor: CollaborationActor {
                        principal_id: Uuid::from_u128(4),
                        agent_id: Some("test-runner".into()),
                    },
                    publisher: signer.public_key().try_into().expect("Ed25519"),
                    authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
                    authority_envelope: envelope,
                },
                completed_at_ms: 1000,
            },
            signer,
        )
    }
    #[test]
    fn evidence_retains_exact_original_signed_fields() {
        let (value, signer) = fixture();
        let record = sign_evidence(&value, &signer).expect("sign");
        assert_eq!(verify_evidence(&record).expect("verify"), value);
        let mut forged = value.clone();
        forged.outcome = CheckOutcome::Failed;
        let mut altered = record.clone();
        altered.canonical_record = forged.encode().expect("encode");
        assert!(
            verify_evidence(&altered).is_err(),
            "outcome must be covered by original signature"
        );
        altered = record.clone();
        altered.signatures.push(altered.signatures[0].clone());
        assert!(
            verify_evidence(&altered).is_err(),
            "multiple signature attribution is ambiguous"
        );
        altered = record;
        altered.format = ACKNOWLEDGEMENT_FORMAT.into();
        assert!(
            verify_evidence(&altered).is_err(),
            "signature domain must remain exact"
        );
    }
    #[test]
    fn acknowledgement_binds_evidence_revision_policy_and_actor() {
        let (value, signer) = fixture();
        let acknowledgement = CheckAcknowledgement {
            version: 1,
            spool: value.spool,
            evidence: value.id,
            evidence_digest: value.id().expect("digest"),
            revision: value.revision,
            policy_version: ContentHash::from_bytes([5; 32]),
            author: value.author,
            client_operation_id: Uuid::from_u128(6),
            occurred_at_ms: 1001,
        };
        let mut record = sign_acknowledgement(&acknowledgement, &signer).expect("sign");
        assert_eq!(
            verify_acknowledgement(&record).expect("verify"),
            acknowledgement
        );
        let mut other = acknowledgement;
        other.policy_version = ContentHash::from_bytes([7; 32]);
        record.canonical_record = other.encode().expect("canonical");
        assert!(
            verify_acknowledgement(&record).is_err(),
            "policy changes require new acknowledgement"
        );
    }
    #[test]
    fn authority_digest_and_artifact_set_are_canonical_and_bounded() {
        let (mut value, signer) = fixture();
        value.author.authority_envelope.push(0);
        assert!(sign_evidence(&value, &signer).is_err());
        value.author.authority_digest =
            ContentHash::compute_typed(AUTHORITY_FORMAT, &value.author.authority_envelope);
        value.artifacts = vec![Uuid::from_u128(8), Uuid::from_u128(8)];
        assert!(sign_evidence(&value, &signer).is_err());
        value.artifacts.clear();
        value.detail = "x".repeat(32769);
        assert!(sign_evidence(&value, &signer).is_err());
    }
}
