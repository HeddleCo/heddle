// SPDX-License-Identifier: Apache-2.0
//! Canonical pack-extent claims — the physical half of the reshape's read path.
//!
//! A manifest node names only immutable logical facts. Where an object's bytes
//! actually live is a *mutable* control-plane fact, so it never enters the
//! content root; repacking must change a read envelope, not a manifest hash.
//! This module gives that envelope a canonical encoding of its own so it can be
//! signed, transported, and checked byte-for-byte.
//!
//! Two rules from the downstream consumer are load-bearing and reproduced here
//! exactly:
//!
//! * **Offset-canonical ordering.** Records are ordered by pack offset, not by
//!   object key. Packs are laid out for delta compression, so object order and
//!   physical order routinely disagree; ordering by object first makes valid
//!   coalesced ranges fail their contiguity check (weft #1070, the bug fixed
//!   after weft #1069 merged).
//! * **Gap-free coverage.** A coalesced range carries a sorted partition of its
//!   authorized records that covers `[start, end)` exactly — no gap, no
//!   overlap. A pack may hold objects of mixed audience, so one physical range
//!   read must never authorize an unselected byte gap between two authorized
//!   records. Coalescing joins *exactly adjacent* extents only.
//!
//! ```text
//! claim: "WPMX" | u8(version=1) | u16(pack_id_len) | pack_id
//!               | u16(etag_len) | etag | u64(start) | u64(end)
//!               | u32(record_count)
//!               | count * ( u8(kind) | [u8;32](object_hash)
//!                         | u64(decoded_size) | u64(offset) | u64(length)
//!                         | [u8;32](encoded_digest) )
//! ```

use crate::object::{
    ContentHash,
    manifest::node::{ManifestKey, ManifestObject, ManifestObjectKind},
};

/// Magic prefix on every canonical pack-range claim.
pub const PACK_CLAIM_MAGIC: [u8; 4] = *b"WPMX";
/// The only claim format version this binary reads or writes.
pub const PACK_CLAIM_VERSION: u8 = 1;

/// One object's physical slice of a pack.
///
/// `encoded_digest` is the BLAKE3 of the *encoded record bytes* — the bytes as
/// they sit in the pack, before decompression or delta resolution. It lets a
/// receiver validate every record independently instead of trusting the whole
/// range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackRecord {
    pub object: ManifestObject,
    /// Byte offset of the encoded record within its pack.
    pub offset: u64,
    /// Encoded length in bytes. Zero-length records are not representable in a
    /// gap-free partition and are rejected by fsck.
    pub length: u64,
    /// `BLAKE3(encoded record bytes)`.
    pub encoded_digest: ContentHash,
}

impl PackRecord {
    pub fn new(
        object: ManifestObject,
        offset: u64,
        length: u64,
        encoded_digest: ContentHash,
    ) -> Self {
        Self {
            object,
            offset,
            length,
            encoded_digest,
        }
    }

    /// Exclusive end offset, or `None` on `u64` overflow.
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }

    pub fn key(&self) -> ManifestKey {
        self.object.key()
    }
}

/// One coalesced physical range read, plus the partition of authorized records
/// it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRangeClaim {
    pub pack_id: String,
    /// Provider entity tag pinning the pack revision this claim was resolved
    /// against. A repack changes the ETag, invalidating the claim without
    /// touching any manifest.
    pub etag: String,
    pub start: u64,
    pub end: u64,
    records: Vec<PackRecord>,
}

impl PackRangeClaim {
    /// Build a claim over `records`, sorting them into offset-canonical order.
    ///
    /// Sorting here is deliberate and is the whole point of weft #1070: the
    /// canonical order is physical, so a pack whose object order differs from
    /// its offset order still produces a contiguous, checkable partition.
    pub fn new(
        pack_id: impl Into<String>,
        etag: impl Into<String>,
        start: u64,
        end: u64,
        mut records: Vec<PackRecord>,
    ) -> Self {
        records.sort_by_key(|record| (record.offset, record.length, record.key()));
        Self {
            pack_id: pack_id.into(),
            etag: etag.into(),
            start,
            end,
            records,
        }
    }

    /// Records in offset-canonical order.
    pub fn records(&self) -> &[PackRecord] {
        &self.records
    }

    /// Total bytes this claim authorizes, or `None` if the range is inverted.
    pub fn byte_len(&self) -> Option<u64> {
        self.end.checked_sub(self.start)
    }

    /// Encode to the single canonical byte string for this claim.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + 1
                + 2
                + self.pack_id.len()
                + 2
                + self.etag.len()
                + 8
                + 8
                + 4
                + self.records.len() * 89,
        );
        out.extend_from_slice(&PACK_CLAIM_MAGIC);
        out.push(PACK_CLAIM_VERSION);
        push_short_string(&mut out, &self.pack_id);
        push_short_string(&mut out, &self.etag);
        out.extend_from_slice(&self.start.to_be_bytes());
        out.extend_from_slice(&self.end.to_be_bytes());
        out.extend_from_slice(&(self.records.len() as u32).to_be_bytes());
        for record in &self.records {
            out.push(record.object.kind.to_byte());
            out.extend_from_slice(record.object.hash.as_bytes());
            out.extend_from_slice(&record.object.decoded_size.to_be_bytes());
            out.extend_from_slice(&record.offset.to_be_bytes());
            out.extend_from_slice(&record.length.to_be_bytes());
            out.extend_from_slice(record.encoded_digest.as_bytes());
        }
        out
    }

    /// The claim's content address — `BLAKE3` of its canonical bytes.
    pub fn address(&self) -> ContentHash {
        ContentHash::compute(&self.encode())
    }

    /// Decode strictly, rejecting truncation, trailing bytes, non-UTF-8 ids,
    /// out-of-order records, and any non-canonical spelling.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackClaimDecodeError> {
        let claim = Self::decode_inner(bytes)?;
        if claim.encode() != bytes {
            return Err(PackClaimDecodeError::NonCanonicalEncoding);
        }
        Ok(claim)
    }

    fn decode_inner(bytes: &[u8]) -> Result<Self, PackClaimDecodeError> {
        let mut reader = ClaimReader { bytes, pos: 0 };

        if reader.take(4)? != PACK_CLAIM_MAGIC {
            return Err(PackClaimDecodeError::BadMagic);
        }
        let version = reader.u8()?;
        if version != PACK_CLAIM_VERSION {
            return Err(PackClaimDecodeError::UnsupportedVersion(version));
        }

        let pack_id = reader.short_string()?;
        let etag = reader.short_string()?;
        let start = reader.u64()?;
        let end = reader.u64()?;
        let count = reader.u32()?;

        let mut records = Vec::with_capacity((count as usize).min(4096));
        let mut previous: Option<(u64, u64, ManifestKey)> = None;
        for _ in 0..count {
            let kind_byte = reader.u8()?;
            let kind = ManifestObjectKind::from_byte(kind_byte)
                .ok_or(PackClaimDecodeError::UnknownObjectKind(kind_byte))?;
            let hash = ContentHash::from_bytes(reader.hash()?);
            let decoded_size = reader.u64()?;
            let offset = reader.u64()?;
            let length = reader.u64()?;
            let encoded_digest = ContentHash::from_bytes(reader.hash()?);
            let object = ManifestObject::new(kind, hash, decoded_size);
            let order = (offset, length, object.key());
            if let Some(prev) = previous
                && prev >= order
            {
                return Err(PackClaimDecodeError::RecordsOutOfOffsetOrder);
            }
            previous = Some(order);
            records.push(PackRecord::new(object, offset, length, encoded_digest));
        }
        if reader.pos != bytes.len() {
            return Err(PackClaimDecodeError::TrailingBytes);
        }

        Ok(Self {
            pack_id,
            etag,
            start,
            end,
            records,
        })
    }
}

struct ClaimReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ClaimReader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], PackClaimDecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(PackClaimDecodeError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(PackClaimDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, PackClaimDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, PackClaimDecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, PackClaimDecodeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, PackClaimDecodeError> {
        let bytes = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(arr))
    }

    fn hash(&mut self) -> Result<[u8; 32], PackClaimDecodeError> {
        let bytes = self.take(32)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(arr)
    }

    fn short_string(&mut self) -> Result<String, PackClaimDecodeError> {
        let len = usize::from(self.u16()?);
        std::str::from_utf8(self.take(len)?)
            .map(str::to_string)
            .map_err(|_| PackClaimDecodeError::InvalidUtf8)
    }
}

fn push_short_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PackClaimDecodeError {
    #[error("claim does not start with the WPMX magic")]
    BadMagic,
    #[error("unsupported pack claim version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown manifest object kind {0}")]
    UnknownObjectKind(u8),
    #[error("claim bytes are truncated")]
    Truncated,
    #[error("claim has trailing bytes after its declared content")]
    TrailingBytes,
    #[error("pack id or etag is not valid UTF-8")]
    InvalidUtf8,
    #[error("records are not strictly ascending by pack offset")]
    RecordsOutOfOffsetOrder,
    #[error("claim bytes are a non-canonical spelling of their own content")]
    NonCanonicalEncoding,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(offset: u64, length: u64, seed: u8) -> PackRecord {
        PackRecord::new(
            ManifestObject::new(
                ManifestObjectKind::Blob,
                ContentHash::from_bytes([seed; 32]),
                length * 2,
            ),
            offset,
            length,
            ContentHash::from_bytes([seed.wrapping_add(100); 32]),
        )
    }

    #[test]
    fn records_canonicalize_by_offset_not_by_object_key() {
        // Object key order (0x01 < 0x02 < 0x03) is the exact reverse of the
        // physical offset order here — the weft #1070 shape.
        let claim = PackRangeClaim::new(
            "pack-a",
            "etag-1",
            0,
            30,
            vec![record(20, 10, 1), record(0, 10, 3), record(10, 10, 2)],
        );
        let offsets: Vec<u64> = claim.records().iter().map(|r| r.offset).collect();
        assert_eq!(offsets, vec![0, 10, 20]);
        let seeds: Vec<u8> = claim
            .records()
            .iter()
            .map(|r| r.object.hash.as_bytes()[0])
            .collect();
        assert_eq!(
            seeds,
            vec![3, 2, 1],
            "object order must not drive the layout"
        );
    }

    #[test]
    fn claim_round_trips_and_is_hash_stable() {
        let claim = PackRangeClaim::new(
            "pack-a",
            "\"etag-xyz\"",
            100,
            140,
            vec![record(120, 20, 2), record(100, 20, 1)],
        );
        let encoded = claim.encode();
        let decoded = PackRangeClaim::decode(&encoded).unwrap();
        assert_eq!(decoded, claim);
        assert_eq!(decoded.encode(), encoded);
        assert_eq!(decoded.address(), claim.address());
    }

    #[test]
    fn decode_rejects_trailing_and_truncated_bytes() {
        let claim = PackRangeClaim::new("p", "e", 0, 10, vec![record(0, 10, 1)]);
        let encoded = claim.encode();

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            PackRangeClaim::decode(&trailing).unwrap_err(),
            PackClaimDecodeError::TrailingBytes
        );
        assert_eq!(
            PackRangeClaim::decode(&encoded[..encoded.len() - 1]).unwrap_err(),
            PackClaimDecodeError::Truncated
        );
    }

    #[test]
    fn decode_rejects_records_written_out_of_offset_order() {
        let claim = PackRangeClaim::new("p", "e", 0, 20, vec![record(0, 10, 1), record(10, 10, 2)]);
        let encoded = claim.encode();
        // Swap the two fixed-width record bodies.
        let header = encoded.len() - 2 * 89;
        let mut swapped = encoded.clone();
        swapped[header..header + 89].copy_from_slice(&encoded[header + 89..]);
        swapped[header + 89..].copy_from_slice(&encoded[header..header + 89]);
        assert_eq!(
            PackRangeClaim::decode(&swapped).unwrap_err(),
            PackClaimDecodeError::RecordsOutOfOffsetOrder
        );
    }

    #[test]
    fn decode_rejects_a_bad_magic_and_version() {
        let encoded = PackRangeClaim::new("p", "e", 0, 10, vec![record(0, 10, 1)]).encode();

        let mut bad_magic = encoded.clone();
        bad_magic[0] = b'Z';
        assert_eq!(
            PackRangeClaim::decode(&bad_magic).unwrap_err(),
            PackClaimDecodeError::BadMagic
        );

        let mut bad_version = encoded;
        bad_version[4] = 9;
        assert_eq!(
            PackRangeClaim::decode(&bad_version).unwrap_err(),
            PackClaimDecodeError::UnsupportedVersion(9)
        );
    }
}
