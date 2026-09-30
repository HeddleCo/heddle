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
    let mut output = Vec::new();
    let mut current = ReplicationOperations::default();
    let mut index = 0;
    for batch in &originals.operations {
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
