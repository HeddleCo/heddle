//! Lazy, independently complete receipt/evidence carriers. A receiver need not
//! retain cross-frame evidence to verify a later original batch.
use std::{collections::BTreeMap, sync::Arc};

use crypto::original_boundary_acceptance::SignedBoundaryAcceptance;
use heddle_object_model::object::{
    ContentHash,
    original_boundary_acceptance::{AdmissionBasis, FORMAT},
};
use prost::Message;

use crate::{contract as wire, replication::store::ReceivedOperation, transport::Error};

struct EncodedOriginal {
    original: wire::SignedRecord,
    receipt: Option<wire::SignedRecord>,
    acceptance: Option<(ContentHash, Arc<SignedBoundaryAcceptance>)>,
}
/// The bound is the encoded ReplicationOperations body, excluding the caller's
/// outer stream frame. Reserve that envelope overhead from negotiated budgets.
/// Input remains lazy; at most one bounded original waits for the next carrier.
pub struct AuthorityBatches<I> {
    input: I,
    pending: Option<EncodedOriginal>,
    max_bytes: usize,
    max_operations: usize,
    ended: bool,
}
pub fn batches<I: IntoIterator<Item = ReceivedOperation>>(
    originals: I,
    max_batch_bytes: usize,
    max_operations: usize,
) -> Result<AuthorityBatches<I::IntoIter>, Error> {
    if max_batch_bytes == 0 || max_batch_bytes > 1024 * 1024 || !(1..=128).contains(&max_operations)
    {
        return Err(Error::Protocol("unsupported authority batch bounds"));
    }
    Ok(AuthorityBatches {
        input: originals.into_iter(),
        pending: None,
        max_bytes: max_batch_bytes,
        max_operations,
        ended: false,
    })
}
impl<I: Iterator<Item = ReceivedOperation>> Iterator for AuthorityBatches<I> {
    type Item = Result<wire::ReplicationOperations, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        match self.next_batch() {
            Ok(Some(batch)) => Some(Ok(batch)),
            Ok(None) => {
                self.ended = true;
                None
            }
            Err(error) => {
                self.ended = true;
                Some(Err(error))
            }
        }
    }
}
impl<I: Iterator<Item = ReceivedOperation>> AuthorityBatches<I> {
    fn next_batch(&mut self) -> Result<Option<wire::ReplicationOperations>, Error> {
        let mut batch = wire::ReplicationOperations::default();
        let mut evidence = BTreeMap::<ContentHash, Arc<SignedBoundaryAcceptance>>::new();
        let mut bytes = 0usize;
        while batch.operations.len() < self.max_operations {
            let candidate = match self.pending.take() {
                Some(value) => value,
                None => match self.input.next() {
                    Some(value) => encode(value)?,
                    None => break,
                },
            };
            if let Some((id, signed)) = &candidate.acceptance
                && let Some(prior) = evidence.get(id)
                && !Arc::ptr_eq(prior, signed)
                && prior.as_ref() != signed.as_ref()
            {
                return Err(Error::Protocol(
                    "conflicting boundary acceptance evidence in outgoing batch",
                ));
            }
            let acceptance = candidate
                .acceptance
                .as_ref()
                .filter(|(id, _)| !evidence.contains_key(id));
            let extra = delimited(candidate.original.encoded_len())
                + candidate
                    .receipt
                    .as_ref()
                    .map_or(0, |value| delimited(value.encoded_len()));
            // Known canonical signature shapes determine wire size without
            // cloning a shared 96 KiB acceptance for every original.
            let acceptance_bytes = acceptance.map_or(0, |(_, signed)| {
                let signature = delimited(32) + delimited(signed.signature.len());
                delimited(
                    delimited(FORMAT.len())
                        + delimited(signed.canonical.len())
                        + delimited(signature),
                )
            });
            let required = bytes.saturating_add(extra).saturating_add(acceptance_bytes);
            if required > self.max_bytes {
                if batch.operations.is_empty() {
                    return Err(Error::Protocol(
                        "original and matched evidence exceed batch budget",
                    ));
                }
                self.pending = Some(candidate);
                break;
            }
            if let Some((id, signed)) = acceptance {
                batch
                    .boundary_acceptances
                    .push(crate::boundary_acceptance::encode(signed)?);
                evidence.insert(*id, signed.clone());
            }
            bytes = required;
            batch.operations.push(candidate.original);
            batch.authority_admissions.extend(candidate.receipt);
        }
        if batch.operations.is_empty() {
            return Ok(None);
        }
        if batch.encoded_len() > self.max_bytes {
            return Err(Error::Protocol(
                "authority batch encoded size differs from bound",
            ));
        }
        Ok(Some(batch))
    }
}
// All fields used here have one-byte protobuf keys, followed by a length varint.
fn delimited(length: usize) -> usize {
    let mut remaining = length;
    let mut prefix = 1;
    while remaining >= 128 {
        prefix += 1;
        remaining >>= 7;
    }
    1 + prefix + length
}
fn encode(received: ReceivedOperation) -> Result<EncodedOriginal, Error> {
    let original = received
        .original
        .verify()
        .map_err(|_| Error::Protocol("invalid original signature for batch"))?;
    let acceptance = if let Some(receipt) = &received.authority_admission {
        let value = receipt
            .verify_signature()
            .map_err(|_| Error::Protocol("invalid receipt signature for batch"))?;
        if value.subject.operation_id()
            != Some(
                original
                    .id()
                    .map_err(|_| Error::Protocol("invalid original identity"))?,
            )
        {
            return Err(Error::Protocol("receipt differs from outgoing original"));
        }
        match (&value.basis, &receipt.boundary_acceptance) {
            (AdmissionBasis::OriginalAuthority, None) => None,
            (AdmissionBasis::BoundaryAcceptance { acceptance }, Some(signed))
                if *acceptance == ContentHash::compute_typed(FORMAT, &signed.canonical) =>
            {
                Some((*acceptance, signed.clone()))
            }
            _ => {
                return Err(Error::Protocol(
                    "outgoing receipt lacks exact basis evidence",
                ));
            }
        }
    } else {
        None
    };
    let record = wire::SignedRecord {
        format: heddle_object_model::object::thread_replication::OPERATION_FORMAT.into(),
        canonical_record: received.original.canonical,
        signatures: vec![wire::RecordSignature {
            public_key: original.publisher.to_vec(),
            signature: received.original.signature,
        }],
    };
    let receipt = received
        .authority_admission
        .as_ref()
        .map(super::encode)
        .transpose()?;
    Ok(EncodedOriginal {
        original: record,
        receipt,
        acceptance,
    })
}
