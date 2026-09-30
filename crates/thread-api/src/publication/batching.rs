// SPDX-License-Identifier: Apache-2.0
//! Bounded client frames within a single, complete source publication.
use prost::Message;

use super::{Error, PublicationOriginals};
use crate::contract::{PublishContentClientFrame, ReplicationOperations};

const BATCH_OPERATIONS: usize = 128;
const BATCH_BYTES: usize = 256 * 1024;
const PUBLICATION_OPERATIONS: usize = 10_000;
const PUBLICATION_BYTES: usize = 16 * 1024 * 1024;

fn fits(operations: usize, bytes: usize, operation_id: &str, frame_limit: usize) -> bool {
    // Operations is one length-delimited field; include the full protobuf
    // wrapper and the caller's operation ID in the negotiated frame budget.
    let frame_bytes = PublishContentClientFrame {
        client_operation_id: operation_id.into(),
        body: None,
    }
    .encoded_len()
        + 1
        + prost::length_delimiter_len(bytes)
        + bytes;
    operations <= BATCH_OPERATIONS && bytes <= BATCH_BYTES && frame_bytes <= frame_limit
}

fn append(batch: &mut ReplicationOperations, unit: ReplicationOperations) {
    batch.operations.extend(unit.operations);
    batch.authority_admissions.extend(unit.authority_admissions);
    for evidence in unit.boundary_acceptances {
        if !batch.boundary_acceptances.contains(&evidence) {
            batch.boundary_acceptances.push(evidence);
        }
    }
}

pub(super) fn bounded_originals(
    originals: &PublicationOriginals,
    operation_id: &str,
    frame_limit: usize,
) -> Result<PublicationOriginals, Error> {
    let operations = originals
        .operations
        .iter()
        .map(|b| b.operations.len())
        .sum();
    if operations > PUBLICATION_OPERATIONS {
        return Err(Error::OriginalBudgetExceeded {
            operations,
            limit_name: "operation count",
            limit: PUBLICATION_OPERATIONS,
            actual: operations,
        });
    }
    if originals.operations.iter().any(|batch| {
        batch.operations.is_empty() || batch.authority_admissions.len() > batch.operations.len()
    }) {
        return Err(Error::Invalid(
            "bounded original genesis and source operations required",
        ));
    }
    let bytes = originals
        .geneses
        .iter()
        .map(Message::encoded_len)
        .sum::<usize>()
        + originals
            .operations
            .iter()
            .map(Message::encoded_len)
            .sum::<usize>();
    if bytes <= PUBLICATION_BYTES
        && originals.operations.iter().all(|batch| {
            fits(
                batch.operations.len(),
                batch.encoded_len(),
                operation_id,
                frame_limit,
            )
        })
    {
        return Ok(originals.clone());
    }
    let mut output = Vec::new();
    let mut current = ReplicationOperations::default();
    let mut index = 0;
    for batch in &originals.operations {
        if batch.operations.len() == 1 && !fits(1, batch.encoded_len(), operation_id, frame_limit) {
            return Err(Error::OriginalOperationTooLarge {
                operation: index + 1,
                bytes: batch.encoded_len(),
                batch_limit: BATCH_BYTES,
                frame_limit,
            });
        }
        // Decode sidecars only when present, to retain the exact receipt and
        // its evidence with the matching original after a frame-size split.
        #[cfg(not(feature = "replication"))]
        if !batch.authority_admissions.is_empty() || !batch.boundary_acceptances.is_empty() {
            return Err(Error::Invalid(
                "publication authority requires replication support",
            ));
        }
        #[cfg(feature = "replication")]
        let received = if batch.authority_admissions.is_empty() {
            if !batch.boundary_acceptances.is_empty() {
                return Err(Error::Invalid("unmatched publication boundary evidence"));
            }
            None
        } else {
            Some(
                crate::authority_admission::match_batch(batch)
                    .map_err(|_| Error::Invalid("invalid publication authority batch"))?,
            )
        };
        for (_position, record) in batch.operations.iter().enumerate() {
            index += 1;
            #[allow(unused_mut)]
            let mut unit = ReplicationOperations {
                operations: vec![record.clone()],
                ..Default::default()
            };
            #[cfg(feature = "replication")]
            if let Some(receipt) = received
                .as_ref()
                .and_then(|items| items.get(_position))
                .and_then(|item| item.authority_admission.as_ref())
            {
                unit.authority_admissions.push(
                    crate::authority_admission::encode(receipt)
                        .map_err(|_| Error::Invalid("invalid publication authority receipt"))?,
                );
                unit.boundary_acceptances =
                    crate::boundary_acceptance::authority_evidence(Some(receipt))
                        .map_err(|_| Error::Invalid("invalid publication boundary evidence"))?;
            }
            if !fits(1, unit.encoded_len(), operation_id, frame_limit) {
                return Err(Error::OriginalOperationTooLarge {
                    operation: index,
                    bytes: unit.encoded_len(),
                    batch_limit: BATCH_BYTES,
                    frame_limit,
                });
            }
            // Repeated protobuf fields are additive. Subtract only exact
            // duplicate evidence that append will retain once in this batch.
            let duplicate_bytes: usize = unit
                .boundary_acceptances
                .iter()
                .filter(|record| current.boundary_acceptances.contains(record))
                .map(|record| {
                    let bytes = record.encoded_len();
                    1 + prost::length_delimiter_len(bytes) + bytes
                })
                .sum();
            let bytes = current.encoded_len() + unit.encoded_len() - duplicate_bytes;
            if !fits(
                current.operations.len() + 1,
                bytes,
                operation_id,
                frame_limit,
            ) {
                output.push(std::mem::take(&mut current));
            }
            append(&mut current, unit);
        }
    }
    if !current.operations.is_empty() {
        output.push(current);
    }
    let bytes = originals
        .geneses
        .iter()
        .map(Message::encoded_len)
        .sum::<usize>()
        + output.iter().map(Message::encoded_len).sum::<usize>();
    if bytes > PUBLICATION_BYTES {
        return Err(Error::OriginalBudgetExceeded {
            operations,
            limit_name: "original metadata bytes",
            limit: PUBLICATION_BYTES,
            actual: bytes,
        });
    }
    Ok(PublicationOriginals {
        geneses: originals.geneses.clone(),
        operations: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{SignedRecord, ThreadGenesisRecord, publish_content_client_frame::Body};

    fn originals(count: usize, record_bytes: usize) -> PublicationOriginals {
        PublicationOriginals {
            geneses: vec![ThreadGenesisRecord::default()],
            operations: vec![ReplicationOperations {
                operations: (0..count)
                    .map(|index| SignedRecord {
                        canonical_record: vec![index as u8; record_bytes],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn batches_by_operation_count_and_exact_encoded_bytes() {
        let input = originals(140, 10);
        let output = bounded_originals(&input, "publication", 512 * 1024).expect("count split");
        assert_eq!(
            output
                .operations
                .iter()
                .map(|batch| batch.operations.len())
                .collect::<Vec<_>>(),
            [128, 12]
        );
        assert_eq!(
            output
                .operations
                .into_iter()
                .flat_map(|b| b.operations)
                .collect::<Vec<_>>(),
            input.operations[0].operations
        );

        // Count alone would allow 128 records. Protobuf field overhead pushes
        // their actual encoded bytes beyond 256 KiB.
        let output = bounded_originals(&originals(128, 2048), "publication", 512 * 1024)
            .expect("byte split");
        assert_eq!(output.operations.len(), 2);
        for batch in output.operations {
            assert!(batch.encoded_len() <= BATCH_BYTES);
        }
        let output =
            bounded_originals(&originals(2, 10), "publication", 512 * 1024).expect("small");
        assert_eq!(output.operations.len(), 1);
    }

    #[test]
    fn negotiated_frames_include_wrapper_and_operation_id() {
        let id = "p".repeat(300);
        let input = originals(12, 300);
        let initial = bounded_originals(&input, &id, 512 * 1024).expect("initial plan");
        assert_eq!(initial.operations.len(), 1);
        let output = bounded_originals(&initial, &id, 1024).expect("Ready narrows frame budget");
        assert_eq!(output.operations.len(), 6);
        for batch in output.operations {
            let frame = PublishContentClientFrame {
                client_operation_id: id.clone(),
                body: Some(Body::Operations(batch)),
            };
            assert!(frame.encoded_len() <= 1024);
        }
    }

    #[cfg(feature = "replication")]
    #[test]
    fn admission_sidecars_stay_with_their_exact_originals() {
        use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
        use heddle_object_model::object::{
            CollaborationActor, ContentHash,
            original_boundary_acceptance::AdmissionBasis,
            thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission},
            thread_replication::{
                OPERATION_FORMAT, ThreadOperation, ThreadOperationBody,
                metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
            },
        };
        use uuid::Uuid;

        use crate::contract::RecordSignature;
        let author = Ed25519Signer::from_seed(&[41; 32]).expect("author");
        let executor = Ed25519Signer::from_seed(&[42; 32]).expect("executor");
        let mut input = originals(0, 0);
        input.operations.clear();
        for index in 1..=3 {
            let control = ThreadControl {
                version: 1,
                spool: Uuid::from_u128(100),
                actor: CollaborationActor {
                    principal_id: Uuid::from_u128(101),
                    agent_id: None,
                },
                authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, b"authority"),
                authority_envelope: b"authority".to_vec(),
                client_operation_id: Uuid::from_u128(index),
                occurred_at_ms: 1,
                control: Control::Name(format!("original {index}")),
            };
            let operation = ThreadOperation {
                version: 1,
                thread: ContentHash::from_bytes([43; 32]),
                parents: Default::default(),
                publisher: author.public_key().try_into().expect("key"),
                body: ThreadOperationBody::Metadata(control.encode().expect("control")),
            };
            let signed = SignedOperation::sign(&operation, &author).expect("original");
            let receipt = ThreadAuthorityAdmission {
                version: 3,
                basis: AdmissionBasis::OriginalAuthority,
                spool: control.spool,
                spool_genesis: ContentHash::from_bytes([44; 32]),
                thread: operation.thread,
                subject: OriginalAuthoritySubject::Operation(operation.id().expect("ID")),
                actor: control.actor,
                publisher: operation.publisher,
                authority_digest: control.authority_digest,
                executor: executor.public_key().try_into().expect("key"),
                admitted_at_ms: 2000,
            };
            let receipt = crate::authority_admission::sign(&receipt, &executor).expect("receipt");
            input.operations.push(ReplicationOperations {
                operations: vec![crate::contract::SignedRecord {
                    format: OPERATION_FORMAT.into(),
                    canonical_record: signed.canonical,
                    signatures: vec![RecordSignature {
                        public_key: operation.publisher.to_vec(),
                        signature: signed.signature,
                    }],
                }],
                authority_admissions: vec![receipt],
                ..Default::default()
            });
        }
        let combined = PublicationOriginals {
            geneses: input.geneses.clone(),
            operations: vec![ReplicationOperations {
                operations: input
                    .operations
                    .iter()
                    .flat_map(|batch| batch.operations.clone())
                    .collect(),
                authority_admissions: input
                    .operations
                    .iter()
                    .flat_map(|batch| batch.authority_admissions.clone())
                    .collect(),
                ..Default::default()
            }],
        };
        let large = bounded_originals(&combined, "publication", 512 * 1024).expect("combined");
        assert_eq!(large.operations.len(), 1);
        let unit_bytes = input.operations[0].encoded_len();
        let smaller =
            bounded_originals(&large, "publication", unit_bytes + 64).expect("split with sidecars");
        assert_eq!(smaller.operations.len(), 3);
        for (batch, original) in smaller.operations.iter().zip(&input.operations) {
            assert_eq!(batch, original);
            assert_eq!(
                crate::authority_admission::match_batch(batch)
                    .expect("exact matched sidecar")
                    .len(),
                1
            );
        }
    }

    #[test]
    fn oversized_single_operation_and_publication_return_typed_errors() {
        for (bytes, frame) in [(BATCH_BYTES, 512 * 1024), (1024, 1024)] {
            let error = bounded_originals(&originals(1, bytes), "publication", frame)
                .err()
                .expect("oversized operation");
            assert!(matches!(
                error,
                Error::OriginalOperationTooLarge { operation: 1, .. }
            ));
        }
        let error = bounded_originals(&originals(10_001, 1), "publication", 512 * 1024)
            .err()
            .expect("operation ceiling");
        assert!(matches!(
            error,
            Error::OriginalBudgetExceeded {
                operations: 10_001,
                limit: 10_000,
                actual: 10_001,
                ..
            }
        ));
        let error = bounded_originals(&originals(100, 170_000), "publication", 512 * 1024)
            .err()
            .expect("metadata ceiling");
        assert!(
            matches!(error, Error::OriginalBudgetExceeded { operations: 100, limit_name: "original metadata bytes", limit: PUBLICATION_BYTES, actual } if actual > PUBLICATION_BYTES)
        );
    }
}
