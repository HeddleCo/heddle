// SPDX-License-Identifier: Apache-2.0

use std::io::Write;

use crate::store::{Result, StoreError};

pub(super) const VERSIONED_HEADER_LEN: usize = 16;
pub(super) const BLAKE3_TRAILER_LEN: usize = 32;

#[derive(Debug, Clone, Copy)]
pub(super) enum HeaderChecksum {
    None,
    Blake3Trailer,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct VersionedHeader {
    pub magic: &'static [u8; 4],
    pub version: u32,
    pub checksum: HeaderChecksum,
    pub too_short: &'static str,
    pub invalid_magic: &'static str,
    pub unsupported_version: &'static str,
    pub checksum_mismatch: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VerifiedHeader {
    pub count: u64,
    pub header_len: usize,
    pub content_end: usize,
}

impl VersionedHeader {
    pub fn write_vec(self, buf: &mut Vec<u8>, count: u64) {
        buf.extend_from_slice(self.magic);
        buf.extend_from_slice(&self.version.to_be_bytes());
        buf.extend_from_slice(&count.to_be_bytes());
    }

    pub fn write_to<W: Write>(self, out: &mut W, count: u64) -> Result<()> {
        out.write_all(self.magic).map_err(StoreError::from)?;
        out.write_all(&self.version.to_be_bytes())
            .map_err(StoreError::from)?;
        out.write_all(&count.to_be_bytes())
            .map_err(StoreError::from)?;
        Ok(())
    }

    pub fn verify(self, data: &[u8]) -> Result<VerifiedHeader> {
        let trailer_len = self.checksum.trailer_len();
        if data.len() < VERSIONED_HEADER_LEN + trailer_len {
            return Err(StoreError::InvalidObject(self.too_short.to_string()));
        }
        if &data[..4] != self.magic {
            return Err(StoreError::InvalidObject(self.invalid_magic.to_string()));
        }

        let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if version != self.version {
            return Err(StoreError::InvalidObject(format!(
                "{}: {}",
                self.unsupported_version, version
            )));
        }

        let content_end = data.len() - trailer_len;
        self.checksum
            .verify(data, content_end, self.checksum_mismatch)?;

        let count = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        Ok(VerifiedHeader {
            count,
            header_len: VERSIONED_HEADER_LEN,
            content_end,
        })
    }
}

impl HeaderChecksum {
    pub fn append(self, buf: &mut Vec<u8>) {
        match self {
            Self::None => {}
            Self::Blake3Trailer => {
                let checksum = blake3::hash(buf);
                buf.extend_from_slice(checksum.as_bytes());
            }
        }
    }

    fn trailer_len(self) -> usize {
        match self {
            Self::None => 0,
            Self::Blake3Trailer => BLAKE3_TRAILER_LEN,
        }
    }

    fn verify(self, data: &[u8], content_end: usize, mismatch: &'static str) -> Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Blake3Trailer => {
                let content = &data[..content_end];
                let stored_checksum = &data[content_end..];
                let computed_checksum = blake3::hash(content);
                if computed_checksum.as_bytes() != stored_checksum {
                    return Err(StoreError::InvalidObject(mismatch.to_string()));
                }
                Ok(())
            }
        }
    }
}
