// SPDX-License-Identifier: Apache-2.0

use super::{
    Result,
    io::{Reader, Writer},
};
use crate::object::ContentHash;

const BLOB_MAGIC: &[u8; 4] = b"HCB2";

/// One verified blob slice within a lineage-solid frame.
pub type DecodedBlob<'a> = (ContentHash, &'a [u8]);

/// Whether `bytes` begin with the lineage-solid blob-frame discriminator.
pub fn is_blob_frame(bytes: &[u8]) -> bool {
    bytes.starts_with(BLOB_MAGIC)
}

/// Encode blob bodies newest-to-oldest in one checksummed solid frame.
///
/// Lengths precede the concatenated bodies, giving the frame reader an exact
/// `(offset, len)` for every object while zstd sees one continuous lineage.
pub fn encode_blob_frame(blobs: &[&[u8]]) -> Result<Vec<u8>> {
    let mut output = Writer::new(BLOB_MAGIC);
    output.put_u64(blobs.len() as u64);
    if let Some((first, rest)) = blobs.split_first() {
        output.put_u64(first.len() as u64);
        let mut previous = i64::try_from(first.len())
            .map_err(|_| super::invalid("blob length exceeds signed delta range"))?;
        for blob in rest {
            let current = i64::try_from(blob.len())
                .map_err(|_| super::invalid("blob length exceeds signed delta range"))?;
            output.put_i64(current - previous);
            previous = current;
        }
    }
    for blob in blobs {
        output.put_fixed(blob);
    }
    Ok(output.finish())
}

/// Decode and whole-frame-verify every indexed blob slice.
pub fn decode_blob_frame(bytes: &[u8]) -> Result<Vec<DecodedBlob<'_>>> {
    let mut input = Reader::verified(bytes, BLOB_MAGIC)?;
    let count = input.get_count("blob frame")?;
    let mut lengths = Vec::with_capacity(count);
    if count > 0 {
        let first = input.get_u64()?;
        lengths.push(checked_length(first)?);
        let mut previous = i64::try_from(first)
            .map_err(|_| super::invalid("blob length exceeds signed delta range"))?;
        for _ in 1..count {
            let current = previous
                .checked_add(input.get_i64()?)
                .ok_or_else(|| super::invalid("blob length delta overflow"))?;
            if current < 0 {
                return Err(super::invalid("blob length delta became negative"));
            }
            lengths.push(checked_length(current as u64)?);
            previous = current;
        }
    }
    let minimum_remaining = lengths
        .iter()
        .try_fold(0usize, |total, len| total.checked_add(*len))
        .ok_or_else(|| super::invalid("blob frame length overflow"))?;
    if input.remaining() != minimum_remaining {
        return Err(super::invalid(format!(
            "blob lengths total {minimum_remaining}, frame has {} body bytes",
            input.remaining()
        )));
    }
    let mut blobs = Vec::with_capacity(count);
    for len in lengths {
        let body = input.take(len)?;
        blobs.push((ContentHash::compute_typed("blob", body), body));
    }
    input.finish()?;
    Ok(blobs)
}

fn checked_length(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| super::invalid("blob length exceeds platform limits"))
}
