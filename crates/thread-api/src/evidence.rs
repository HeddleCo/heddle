//! Typed immutable evidence codecs shared by hosted and device callers.
//! Signature verification does not establish current account or Spool authority.
use crypto::{Ed25519Signer, Signer};
pub use heddle_object_model::object::check_evidence::{
    CheckAcknowledgement, CheckAuthor, CheckEvidence, CheckOutcome,
};
use heddle_object_model::object::{
    ContentHash,
    check_evidence::{ACKNOWLEDGEMENT_FORMAT, EVIDENCE_FORMAT},
};
use prost::Message;

use crate::{contract as wire, transport::Error};

/// Stable version of the exact signed receipt, including its original signature.
pub fn record_version(record: &wire::SignedRecord) -> ContentHash {
    ContentHash::compute_typed("heddle-signed-check-receipt-v1", &record.encode_to_vec())
}

/// Derive the complete query/display surface from the verified original.
/// Current authority and visibility remain the receiving endpoint's responsibility.
pub fn project(record: &wire::SignedRecord) -> Result<wire::EvidenceRecord, Error> {
    let value = verify_evidence(record)?;
    let spool = wire::SpoolRef {
        id: value.spool.to_string(),
    };
    Ok(wire::EvidenceRecord {
        r#ref: Some(wire::RecordRef {
            spool: Some(spool.clone()),
            id: value.id.to_string(),
        }),
        thread: Some(wire::ThreadRef {
            spool: Some(spool.clone()),
            id: Some(wire::ThreadId {
                value: value.thread.as_bytes().to_vec(),
            }),
        }),
        version: record_version(record).as_bytes().to_vec(),
        revision: Some(wire::RevisionRef {
            spool: Some(spool),
            revision: Some(wire::revision_ref::Revision::State(
                api::heddle::api::v1alpha1::StateId {
                    value: value.revision.as_bytes().to_vec(),
                },
            )),
        }),
        check: value.check.clone(),
        evidence: Some(record.clone()),
        coverage: wire::Coverage::Complete as i32,
        summary: Some(summary(&value)),
    })
}

/// Presentation only: call `verify_evidence` before projecting untrusted bytes.
pub fn summary(value: &CheckEvidence) -> wire::CheckEvidenceSummary {
    let reference = |id: &uuid::Uuid| wire::RecordRef {
        spool: Some(wire::SpoolRef {
            id: value.spool.to_string(),
        }),
        id: id.to_string(),
    };
    wire::CheckEvidenceSummary {
        outcome: match value.outcome {
            CheckOutcome::Passed => wire::check_evidence_summary::Outcome::Passed,
            CheckOutcome::Failed => wire::check_evidence_summary::Outcome::Failed,
            CheckOutcome::Error => wire::check_evidence_summary::Outcome::Error,
            CheckOutcome::Skipped => wire::check_evidence_summary::Outcome::Skipped,
        } as i32,
        detail: value.detail.clone(),
        author: Some(wire::PrincipalRef {
            id: value.author.actor.principal_id.to_string(),
        }),
        agent_id: value.author.actor.agent_id.clone().unwrap_or_default(),
        artifacts: value.artifacts.iter().map(reference).collect(),
        supersedes: value.supersedes.iter().map(reference).collect(),
        completed_at: Some(prost_types::Timestamp {
            seconds: value.completed_at_ms.div_euclid(1000),
            nanos: (value.completed_at_ms.rem_euclid(1000) * 1_000_000) as i32,
        }),
    }
}

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
/// Project original progress without treating it as a successful check outcome.
pub fn project_acknowledgement(
    record: &wire::SignedRecord,
) -> Result<wire::CheckAcknowledgementRecord, Error> {
    let value = verify_acknowledgement(record)?;
    let spool = wire::SpoolRef {
        id: value.spool.to_string(),
    };
    Ok(wire::CheckAcknowledgementRecord {
        r#ref: Some(wire::RecordRef {
            spool: Some(spool.clone()),
            id: value.client_operation_id.to_string(),
        }),
        version: record_version(record).as_bytes().to_vec(),
        evidence: Some(wire::RecordRef {
            spool: Some(spool.clone()),
            id: value.evidence.to_string(),
        }),
        revision: Some(wire::RevisionRef {
            spool: Some(spool),
            revision: Some(wire::revision_ref::Revision::State(
                api::heddle::api::v1alpha1::StateId {
                    value: value.revision.as_bytes().to_vec(),
                },
            )),
        }),
        policy_version: value.policy_version.as_bytes().to_vec(),
        author: Some(wire::PrincipalRef {
            id: value.author.actor.principal_id.to_string(),
        }),
        agent_id: value.author.actor.agent_id.unwrap_or_default(),
        acknowledged_at: Some(prost_types::Timestamp {
            seconds: value.occurred_at_ms.div_euclid(1000),
            nanos: (value.occurred_at_ms.rem_euclid(1000) * 1_000_000) as i32,
        }),
        acknowledgement: Some(record.clone()),
    })
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

    #[test]
    fn rust_v2_interop_vector() {
        let (mut value, _) = fixture();
        let signer = Ed25519Signer::from_seed(&[19; 32]).expect("fixed vector seed");
        value.author.publisher = signer.public_key().try_into().expect("Ed25519 key");
        value.artifacts = vec![Uuid::from_u128(8)];
        value.supersedes = vec![Uuid::from_u128(9)];
        value.completed_at_ms = 1234;
        let evidence = sign_evidence(&value, &signer).expect("evidence");
        let ack = CheckAcknowledgement {
            version: 1,
            spool: value.spool,
            evidence: value.id,
            evidence_digest: value.id().expect("digest"),
            revision: value.revision,
            policy_version: ContentHash::from_bytes([5; 32]),
            author: value.author.clone(),
            client_operation_id: Uuid::from_u128(6),
            occurred_at_ms: 2001,
        };
        let ack = sign_acknowledgement(&ack, &signer).expect("acknowledgement");
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        println!("RUST_V2_EVIDENCE_HEX {}", hex(&evidence.canonical_record));
        println!(
            "RUST_V2_EVIDENCE_SIGNATURE {}",
            hex(&evidence.signatures[0].signature)
        );
        println!("RUST_V2_ACKNOWLEDGEMENT_HEX {}", hex(&ack.canonical_record));
        println!(
            "RUST_V2_ACKNOWLEDGEMENT_SIGNATURE {}",
            hex(&ack.signatures[0].signature)
        );
    }

    fn fixture() -> (CheckEvidence, Ed25519Signer) {
        let signer = Ed25519Signer::generate().expect("signer");
        let envelope = b"independently verified by receiving host".to_vec();
        (
            CheckEvidence {
                version: 2,
                thread: ContentHash::from_bytes([7; 32]),
                id: Uuid::from_u128(1),
                spool: Uuid::from_u128(2),
                revision: StateId::from_bytes([3; 32]),
                check: "unit-tests".into(),
                outcome: CheckOutcome::Passed,
                detail: "42 passed".into(),
                artifacts: Vec::new(),
                supersedes: Vec::new(),
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
    fn projection_preserves_typed_attribution_references_and_outcomes() {
        let (mut value, signer) = fixture();
        value.artifacts = vec![Uuid::from_u128(8)];
        value.supersedes = vec![Uuid::from_u128(9)];
        value.completed_at_ms = 1234;
        for (outcome, expected) in [
            (
                CheckOutcome::Passed,
                wire::check_evidence_summary::Outcome::Passed,
            ),
            (
                CheckOutcome::Failed,
                wire::check_evidence_summary::Outcome::Failed,
            ),
            (
                CheckOutcome::Error,
                wire::check_evidence_summary::Outcome::Error,
            ),
            (
                CheckOutcome::Skipped,
                wire::check_evidence_summary::Outcome::Skipped,
            ),
        ] {
            value.outcome = outcome;
            let record = sign_evidence(&value, &signer).expect("signed evidence");
            let projected = project(&record).expect("verified projection");
            assert_eq!(projected.version, record_version(&record).as_bytes());
            let summary = projected.summary.expect("typed summary");
            assert_eq!(summary.outcome, expected as i32);
            assert_eq!(summary.detail, "42 passed");
            assert_eq!(
                summary.author.expect("original author").id,
                value.author.actor.principal_id.to_string()
            );
            assert_eq!(summary.agent_id, "test-runner");
            assert_eq!(summary.artifacts[0].id, value.artifacts[0].to_string());
            assert_eq!(
                summary.artifacts[0].spool.as_ref().expect("scope").id,
                value.spool.to_string()
            );
            assert_eq!(summary.supersedes[0].id, value.supersedes[0].to_string());
            let completed = summary.completed_at.expect("completion");
            assert_eq!((completed.seconds, completed.nanos), (1, 234_000_000));
            let mut forged = record;
            forged.signatures[0].signature[0] ^= 1;
            assert!(
                project(&forged).is_err(),
                "unverified bytes cannot become display evidence"
            );
        }
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
        let projected = project_acknowledgement(&record).expect("typed progress");
        assert_eq!(
            projected.evidence.expect("evidence").id,
            acknowledgement.evidence.to_string()
        );
        assert_eq!(
            projected.policy_version,
            acknowledgement.policy_version.as_bytes()
        );
        assert_eq!(
            projected.author.expect("original author").id,
            acknowledgement.author.actor.principal_id.to_string()
        );
        assert_eq!(projected.agent_id, "test-runner");
        assert_eq!(projected.acknowledgement.as_ref(), Some(&record));
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
        value.supersedes = vec![value.id];
        assert!(sign_evidence(&value, &signer).is_err());
        value.supersedes.clear();
        value.detail = "x".repeat(32769);
        assert!(sign_evidence(&value, &signer).is_err());
    }
}
