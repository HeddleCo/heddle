// SPDX-License-Identifier: Apache-2.0
//! NPK1 settled-tree packs.
//!
//! NPK1 is deliberately separate from the capture-time HLR1/HDC1 objects.
//! A background repack builds one immutable file containing the shared
//! dictionaries, restartable records, both lookup tables, and a checksummed
//! chunk manifest. Readers mmap that file and verify only the chunks needed to
//! resolve one indexed tree or entry; staged publication verifies every chunk.

mod builder;
mod codec;
mod reader;

#[cfg(test)]
mod tests;

pub(super) use builder::{Npk1Build, Npk1BuildError, build_npk1_pack};
pub(super) use reader::{Npk1Manager, Npk1Pack};

use crate::store::{HeddleError, Result};

const MAGIC: &[u8; 4] = b"NPK1";
const INDEX_MAGIC: &[u8; 4] = b"NPI1";
const NAME_MAGIC: &[u8; 4] = b"NDI1";
const TARGET_MAGIC: &[u8; 4] = b"TDI1";
const TRAILER_MAGIC: &[u8; 4] = b"NPT1";
const VERSION: u32 = 2;
const HEADER_LEN: usize = 64;
const CHECKSUM_LEN: usize = 32;
const CHECKSUM_CHUNK_BYTES: usize = 64 * 1024;
const TRAILER_HEADER_LEN: usize = 16;
const NAME_RESTART: usize = 128;
const RECORD_BLOCK_ENTRIES: usize = 128;
#[cfg(feature = "zstd")]
const RECORD_DICTIONARY_MAX_BYTES: usize = 32 * 1024;
#[cfg(feature = "zstd")]
const RECORD_DICTIONARY_MIN_BYTES: usize = 16 * 1024;
const WINDOW_BUCKET_LIMIT: usize = 64;
const EXACT_CANDIDATES: usize = 16;
const MAX_CHAIN_DEPTH: usize = 16;
#[cfg(feature = "zstd")]
const RECORD_LEVEL: i32 = 10;
const LARGE_OFFSET_FLAG: u32 = 1 << 31;

fn invalid(message: impl Into<String>) -> HeddleError {
    HeddleError::InvalidObject(format!("NPK1: {}", message.into()))
}

fn checked_slice<'a>(bytes: &'a [u8], offset: usize, len: usize, what: &str) -> Result<&'a [u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| invalid(format!("{what} range overflow")))?;
    bytes
        .get(offset..end)
        .ok_or_else(|| invalid(format!("truncated {what}")))
}

fn read_u16(bytes: &[u8], offset: usize, what: &str) -> Result<u16> {
    let raw: [u8; 2] = checked_slice(bytes, offset, 2, what)?
        .try_into()
        .map_err(|_| invalid(format!("invalid {what}")))?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize, what: &str) -> Result<u32> {
    let raw: [u8; 4] = checked_slice(bytes, offset, 4, what)?
        .try_into()
        .map_err(|_| invalid(format!("invalid {what}")))?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize, what: &str) -> Result<u64> {
    let raw: [u8; 8] = checked_slice(bytes, offset, 8, what)?
        .try_into()
        .map_err(|_| invalid(format!("invalid {what}")))?;
    Ok(u64::from_le_bytes(raw))
}

fn usize_from_u64(value: u64, what: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| invalid(format!("{what} exceeds address space")))
}

fn put_varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take_varint(bytes: &[u8], offset: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| invalid("truncated varint"))?;
        *offset += 1;
        if shift >= usize::BITS || ((byte & 0x7f) as usize) > (usize::MAX >> shift) {
            return Err(invalid("varint overflow"));
        }
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn shared_prefix(left: &str, right: &str) -> usize {
    let mut prefix = left
        .as_bytes()
        .iter()
        .zip(right.as_bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while !left.is_char_boundary(prefix) {
        prefix -= 1;
    }
    prefix
}
