//! Allocation-free protobuf shape checks for live frames, before prost builds
//! repeated-message vectors. This bounds protobuf framing and retained input;
//! it is not a bound on arbitrary canonical MessagePack object decoding.
use std::mem::size_of;

use crate::{contract::*, replication::opening::FRAME_LIMIT, transport};

// Complete recursive message vocabulary reachable through ErrorDetail/StreamFailure
// and TransferObject. Inline timestamp/duration storage is included in its parent.
// Each heap-bearing protobuf node needs at least a tag+length (two wire bytes).
// Four times the largest layout per wire byte covers Vec's minimum/growth
// capacity and coexistence of an old/new singular value during duplicate merge.
// Payload byte/string copies are additionally covered by the base reservation.
const OPAQUE_NODE_SIZES: &[usize] = &[
    size_of::<api::heddle::api::v1alpha1::CallFailure>(),
    size_of::<api::heddle::api::v1alpha1::ErrorDetail>(),
    size_of::<api::heddle::api::v1alpha1::RetryAdvice>(),
    size_of::<api::heddle::api::v1alpha1::ConflictDetail>(),
    size_of::<api::heddle::api::v1alpha1::CursorFailure>(),
    size_of::<api::heddle::api::v1alpha1::CapabilityRequirement>(),
    size_of::<api::heddle::api::v1alpha1::PolicyDenial>(),
    size_of::<api::heddle::api::v1alpha1::UnknownDetail>(),
    size_of::<api::heddle::api::v1alpha1::AmbiguousChangeIdDetail>(),
    size_of::<api::heddle::api::v1alpha1::SignupFailure>(),
    size_of::<api::heddle::api::v1alpha1::StreamFailure>(),
    size_of::<api::heddle::api::v1alpha1::HumanVerificationChallenge>(),
    size_of::<api::heddle::api::v1alpha1::OAuthLinkChallenge>(),
    size_of::<TransferObject>(),
    size_of::<ObjectAddress>(),
    size_of::<String>(),
    size_of::<Vec<u8>>(),
];
fn opaque_node_factor() -> usize {
    OPAQUE_NODE_SIZES.iter().copied().max().unwrap_or(0) * 4
}

type Result<T> = std::result::Result<T, transport::Error>;
fn invalid() -> transport::Error {
    transport::Error::Protocol("live replication input shape exceeds bounds")
}
struct Field<'a> {
    tag: u32,
    data: Option<&'a [u8]>,
}
fn varint(bytes: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or_else(invalid)?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return Err(invalid());
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(invalid())
}
fn field<'a>(bytes: &mut &'a [u8]) -> Result<Option<Field<'a>>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let key = varint(bytes)?;
    let tag = u32::try_from(key >> 3).map_err(|_| invalid())?;
    if tag == 0 {
        return Err(invalid());
    }
    let data = match key & 7 {
        0 => {
            varint(bytes)?;
            None
        }
        1 | 5 => {
            let n = if key & 7 == 1 { 8 } else { 4 };
            *bytes = bytes.get(n..).ok_or_else(invalid)?;
            None
        }
        2 => {
            let n = usize::try_from(varint(bytes)?).map_err(|_| invalid())?;
            let value = bytes.get(..n).ok_or_else(invalid)?;
            *bytes = &bytes[n..];
            Some(value)
        }
        _ => return Err(invalid()),
    };
    Ok(Some(Field { tag, data }))
}
fn data<'a>(field: &Field<'a>) -> Result<&'a [u8]> {
    field.data.ok_or_else(invalid)
}
fn count(value: &mut usize, max: usize) -> Result<()> {
    *value += 1;
    if *value > max { Err(invalid()) } else { Ok(()) }
}
fn id(field: &Field<'_>) -> Result<()> {
    if data(field)?.len() == 32 {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn signature(mut bytes: &[u8]) -> Result<()> {
    let (mut keys, mut signatures) = (0, 0);
    while let Some(f) = field(&mut bytes)? {
        match f.tag {
            1 => {
                count(&mut keys, 1)?;
                id(&f)?;
            }
            2 => {
                count(&mut signatures, 1)?;
                if data(&f)?.len() != 64 {
                    return Err(invalid());
                }
            }
            _ => {}
        }
    }
    if keys == 1 && signatures == 1 {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn record(mut bytes: &[u8]) -> Result<()> {
    let (mut formats, mut canonicals, mut signatures) = (0, 0, 0);
    while let Some(f) = field(&mut bytes)? {
        match f.tag {
            1 => {
                count(&mut formats, 1)?;
                if data(&f)?.len() > 128 {
                    return Err(invalid());
                }
            }
            2 => {
                count(&mut canonicals, 1)?;
                data(&f)?;
            }
            3 => {
                count(&mut signatures, 1)?;
                signature(data(&f)?)?;
            }
            _ => {}
        }
    }
    if formats == 1 && canonicals == 1 && signatures == 1 {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn frontier(mut bytes: &[u8], heads: &mut usize, max: usize) -> Result<()> {
    while let Some(f) = field(&mut bytes)? {
        if f.tag == 2 {
            count(heads, max)?;
            id(&f)?;
        }
    }
    Ok(())
}
fn failure(mut bytes: &[u8], opaque: &mut usize) -> Result<()> {
    while let Some(f) = field(&mut bytes)? {
        match f.tag {
            2 => {
                data(&f)?;
            }
            // Preserve the complete published ErrorDetail contract. It is not
            // interpreted as source authority; allow conservative decode space
            // for its nested protobuf structures rather than discarding it.
            4 => {
                *opaque = opaque.checked_add(data(&f)?.len()).ok_or_else(invalid)?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn rejection(mut bytes: &[u8], opaque: &mut usize) -> Result<()> {
    while let Some(f) = field(&mut bytes)? {
        match f.tag {
            1 => id(&f)?,
            2 => failure(data(&f)?, opaque)?,
            _ => {}
        }
    }
    Ok(())
}
/// A conservative reservation for validated protobuf framing, transient wire
/// copies during matching, and retained decoded originals/sidecars. Canonical
/// ThreadOperation decoding remains subject to its independent model limits.
pub fn reservation(bytes: &[u8], max: usize) -> Result<usize> {
    if max == 0 || max > 64 || bytes.len() > FRAME_LIMIT {
        return Err(invalid());
    }
    let original_len = bytes.len();
    let mut bytes = bytes;
    let mut body_count = 0;
    let mut objects = 0usize;
    let mut opaque = 0usize;
    while let Some(body) = field(&mut bytes)? {
        if !(2..=5).contains(&body.tag) {
            return Err(invalid());
        }
        count(&mut body_count, 1)?;
        let mut inner = data(&body)?;
        let (mut first, mut second, mut third, mut fourth, mut fifth) = (0, 0, 0, 0, 0);
        let mut heads = 0;
        while let Some(f) = field(&mut inner)? {
            match (body.tag, f.tag) {
                (2, 1) => {
                    count(&mut first, max)?;
                    frontier(data(&f)?, &mut heads, max)?;
                    objects += 1;
                }
                (3, 1) => {
                    count(&mut first, max)?;
                    id(&f)?;
                    objects += 1;
                }
                (4, 1) => {
                    count(&mut first, max)?;
                    record(data(&f)?)?;
                    objects += 3;
                }
                (4, 2) => {
                    count(&mut second, max)?;
                    record(data(&f)?)?;
                    objects += 3;
                }
                (4, 3) => {
                    count(&mut third, max)?;
                    record(data(&f)?)?;
                    objects += 3;
                }
                (5, 1) | (5, 2) => {
                    count(&mut first, max)?;
                    id(&f)?;
                    objects += 1;
                }
                (5, 3) => {
                    count(&mut first, max)?;
                    count(&mut third, max)?;
                    rejection(data(&f)?, &mut opaque)?;
                    objects += 3;
                }
                (5, 4) => {
                    count(&mut fourth, max)?;
                    frontier(data(&f)?, &mut heads, max)?;
                    objects += 1;
                }
                (5, 5) => {
                    count(&mut fifth, max)?;
                    opaque = opaque.checked_add(data(&f)?.len()).ok_or_else(invalid)?;
                    objects += 1;
                }
                (5, 6) => {
                    data(&f)?;
                }
                _ => {}
            }
        }
    }
    if body_count != 1 {
        return Err(invalid());
    }
    let container = size_of::<crate::replication::Frame>()
        + size_of::<crate::replication::InputUnit>()
        + size_of::<SignedRecord>()
        + size_of::<RecordSignature>()
        + size_of::<ReplicationRejection>();
    original_len
        .checked_mul(6)
        .and_then(|value| value.checked_add((objects + 4) * 4 * container))
        .and_then(|value| value.checked_add(opaque.saturating_mul(opaque_node_factor())))
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;
    fn record() -> SignedRecord {
        SignedRecord {
            format: "fixture".into(),
            canonical_record: vec![1; 32],
            signatures: vec![RecordSignature {
                public_key: vec![2; 32],
                signature: vec![3; 64],
            }],
        }
    }
    fn batch(operations: Vec<SignedRecord>, authority_admissions: Vec<SignedRecord>) -> Vec<u8> {
        ReplicateThreadRequest {
            body: Some(replicate_thread_request::Body::Operations(
                ReplicationOperations {
                    operations,
                    authority_admissions,
                    boundary_acceptances: vec![],
                },
            )),
        }
        .encode_to_vec()
    }
    #[test]
    fn protobuf_shape_preflight_bounds_record_expansion_before_decode() {
        let valid = record();
        let bytes = batch(vec![valid.clone(); 64], vec![valid.clone(); 64]);
        assert!(
            reservation(&bytes, 64).expect("maximum separate record and receipt vectors")
                > bytes.len()
        );
        assert!(
            reservation(&bytes, 63).is_err(),
            "negotiated count applies before protobuf decode"
        );
        let mut duplicate = valid.clone();
        duplicate.signatures.push(valid.signatures[0].clone());
        assert!(
            reservation(&batch(vec![duplicate], vec![]), 64).is_err(),
            "multiple nested signatures must fail shape preflight"
        );
        let mut short = valid;
        short.signatures[0].signature.truncate(63);
        assert!(
            reservation(&batch(vec![short], vec![]), 64).is_err(),
            "signature shape is checked without allocating decoded vectors"
        );
        assert!(reservation(&[0x22, 0xff], 64).is_err(), "truncated length");
    }
    #[test]
    fn protobuf_shape_bounds_boundary_acceptance_carriers() {
        let valid = record();
        let encode = |records| {
            ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Operations(
                    ReplicationOperations {
                        operations: vec![valid.clone()],
                        authority_admissions: vec![],
                        boundary_acceptances: records,
                    },
                )),
            }
            .encode_to_vec()
        };
        let maximum = encode(vec![valid.clone(); 64]);
        assert!(reservation(&maximum, 64).expect("bounded acceptance records") > maximum.len());
        assert!(
            reservation(&maximum, 63).is_err(),
            "acceptance count obeys negotiated bound"
        );
        let mut duplicate = valid.clone();
        duplicate.signatures.push(valid.signatures[0].clone());
        assert!(
            reservation(&encode(vec![duplicate]), 64).is_err(),
            "acceptance signature expansion is rejected before decode"
        );
    }
    #[test]
    fn protobuf_shape_preserves_legal_receipt_details_and_accounts_nested_layouts() {
        use api::heddle::api::v1alpha1::{
            CallFailure, CapabilityRequirement, ErrorDetail, error_detail::Context,
        };
        let failure = CallFailure {
            code: 9,
            message: "needs a capability".into(),
            error: Some(ErrorDetail {
                context: Some(Context::Capability(CapabilityRequirement {
                    capabilities: vec![String::new(); 128],
                })),
                ..Default::default()
            }),
        };
        let receipt = ReplicationReceipt {
            rejected: vec![ReplicationRejection {
                operation_id: vec![7; 32],
                failure: Some(failure),
            }],
            accepted_frontiers: vec![CausalFrontier {
                facet: 1,
                heads: vec![vec![8; 32]],
            }],
            missing_objects: vec![TransferObject {
                address: Some(ObjectAddress {
                    algorithm: "blake3".into(),
                    digest: vec![9; 32],
                }),
                kind: "source".into(),
                ..Default::default()
            }],
            sharing_policy_version: vec![5; 32],
            ..Default::default()
        };
        let response = ReplicateThreadResponse {
            body: Some(replicate_thread_response::Body::Receipt(receipt)),
        };
        let bytes = response.encode_to_vec();
        let reserved = reservation(&bytes, 64)
            .expect("preserve all existing receipt capability and coverage fields");
        assert_eq!(
            ReplicateThreadResponse::decode(bytes.as_slice()).expect("prost response"),
            response
        );
        assert_eq!(
            OPAQUE_NODE_SIZES.len(),
            17,
            "all currently recursive error/transfer node layouts enumerated"
        );
        assert!(
            OPAQUE_NODE_SIZES
                .iter()
                .all(|size| opaque_node_factor() >= 4 * size)
        );
        assert!(
            reserved > 128 * size_of::<String>() + bytes.len(),
            "empty repeated strings still consume vector slots"
        );
    }
}
